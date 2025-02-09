use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::executor::block_on;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use futures_lite::FutureExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::spawn;
use tokio::sync::{Mutex, RwLock};
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::{Error, Message};
use tokio_tungstenite::{accept_hdr_async, WebSocketStream};

type Rx = SplitStream<WebSocketStream<TcpStream>>;
type Tx = SplitSink<WebSocketStream<TcpStream>, Message>;

static APPLICATIONS_RX: LazyLock<Mutex<HashMap<String, Option<Rx>>>> =
	LazyLock::new(|| Mutex::new(HashMap::new()));
static APPLICATIONS_TX: LazyLock<Mutex<HashMap<String, Option<Tx>>>> =
	LazyLock::new(|| Mutex::new(HashMap::new()));

#[tokio::main]
async fn main() {
	#[cfg(not(feature = "self-host"))]
	let stripe = stripe::Client::new(std::env::var("STRIPE_SECRET_KEY").unwrap());

	let listener = TcpListener::bind("0.0.0.0:8402").await.unwrap();

	let handle_connection =
		|ws_stream: WebSocketStream<TcpStream>, from: String, id: String| match &from[..] {
			"plugin" => {
				spawn(async move {
					let (mut ptx, mut prx) = ws_stream.split();

					APPLICATIONS_RX.lock().await.insert(id.clone(), None);
					APPLICATIONS_TX.lock().await.insert(id.clone(), None);
					let id_2 = id.clone();

					let should_close_plugin = Arc::new(RwLock::new(false));
					let should_close_plugin_2 = should_close_plugin.clone();
					let should_close_application = Arc::new(RwLock::new(false));
					let should_close_application_2 = should_close_application.clone();

					spawn(async move {
						let mut atx: Option<Tx> = None;
						while let Some(message) = prx
							.next()
							.or(async {
								while !*should_close_application.read().await {
									sleep(Duration::from_millis(5)).await;
								}
								*should_close_application.write().await = false;
								Some(Err(Error::Io(std::io::Error::new(
									std::io::ErrorKind::BrokenPipe,
									"application connection closed",
								))))
							})
							.await
						{
							let Ok(message) = message else {
								if let Err(Error::Io(error)) = message {
									if error.kind() == std::io::ErrorKind::BrokenPipe {
										atx = None;
										APPLICATIONS_TX.lock().await.insert(id.clone(), None);
									}
								};
								continue;
							};
							if let Some(catx) = &mut atx {
								if catx.send(message).await.is_err() {
									atx = None;
									APPLICATIONS_TX.lock().await.insert(id.clone(), None);
								}
							} else {
								let mut lock = APPLICATIONS_TX.lock().await;
								if let Some(Some(_)) = lock.get(&id) {
									atx = lock.remove(&id).unwrap();
									if atx.as_mut().unwrap().send(message).await.is_err() {
										atx = None;
										lock.insert(id.clone(), None);
									}
								}
							}
						}
						APPLICATIONS_TX.lock().await.remove(&id);
						*should_close_plugin.write().await = true;
					});

					spawn(async move {
						let mut arx: Option<Rx> = None;
						'outer: loop {
							if arx.is_some() {
								while let Some(message) = arx
									.as_mut()
									.unwrap()
									.next()
									.or(async {
										while !*should_close_plugin_2.read().await {
											sleep(Duration::from_millis(5)).await;
										}
										*should_close_plugin_2.write().await = false;
										Some(Err(Error::Io(std::io::Error::new(
											std::io::ErrorKind::BrokenPipe,
											"plugin connection closed",
										))))
									})
									.await
								{
									let Ok(message) = message else {
										if let Err(Error::Io(error)) = message {
											if error.kind() == std::io::ErrorKind::BrokenPipe {
												break 'outer;
											}
										};
										continue;
									};
									if let Message::Close(_) = message {
										break;
									}
									if ptx.send(message).await.is_err() {
										break 'outer;
									}
								}
								arx = None;
								APPLICATIONS_RX.lock().await.insert(id_2.clone(), None);
								*should_close_application_2.write().await = true;
							} else {
								let mut lock = APPLICATIONS_RX.lock().await;
								if let Some(Some(_)) = lock.get(&id_2) {
									arx = lock.remove(&id_2).unwrap();
								} else {
									sleep(Duration::from_millis(5)).await;
								}
							}
						}
						APPLICATIONS_RX.lock().await.remove(&id_2);
					});
				});
			}
			"application" => {
				spawn(async move {
					let (mut ltx, mut lrx) =
						(APPLICATIONS_TX.lock().await, APPLICATIONS_RX.lock().await);
					let (htx, hrx) = (ltx.get_mut(&id), lrx.get_mut(&id));
					if htx.is_some() && hrx.is_some() {
						let (atx, arx) = ws_stream.split();
						*(htx.unwrap()) = Some(atx);
						*(hrx.unwrap()) = Some(arx);
					}
				});
			}
			_ => panic!("invalid connection type"),
		};

	while let Ok((stream, _)) = listener.accept().await {
		let mut details = None;
		let callback = |req: &Request, res: Response| -> Result<Response, ErrorResponse> {
			fn error(status: u16, body: String) -> ErrorResponse {
				Response::builder().status(status).body(Some(body)).unwrap()
			}

			let path = req.uri().path().to_string();
			let segments = path
				.split('/')
				.filter(|v| !v.is_empty())
				.map(|v| v.to_owned())
				.collect::<Vec<_>>();
			if segments.len() < 3 {
				return Err(error(400, "not enough path segments".to_owned()));
			}

			#[cfg(not(feature = "self-host"))]
			{
				use std::str::FromStr;
				let customer_id = stripe::CustomerId::from_str(&segments[1])
					.map_err(|e| error(400, format!("invalid customer ID: {e}")))?;
				let customer = block_on(stripe::Customer::retrieve(&stripe, &customer_id, &[]))
					.map_err(|e| error(409, format!("failed to retrieve customer: {e}")))?;

				let metadata = customer
					.metadata
					.ok_or_else(|| error(500, "no metadata for customer".to_owned()))?;

				match &segments[0][..] {
					"plugin" => {
						let object = serde_json::from_str::<serde_json::Value>(
							metadata.get("pluginSessions").ok_or_else(|| {
								error(500, "no plugin sessions object on customer".to_owned())
							})?,
						)
						.map_err(|e| {
							error(500, format!("failed to parse plugin sessions object: {e}"))
						})?;
						let sessions = object
							.as_object()
							.ok_or_else(|| {
								error(500, "plugin sessions value is not an object".to_owned())
							})?
							.keys()
							.collect::<Vec<_>>();
						if !sessions.contains(&&segments[2]) {
							return Err(error(409, "invalid plugin session".to_owned()));
						}
					}
					"application" => {
						let object = serde_json::from_str::<serde_json::Value>(
							metadata.get("sessions").ok_or_else(|| {
								error(500, "no sessions object on customer".to_owned())
							})?,
						)
						.map_err(|e| error(500, format!("failed to parse sessions object: {e}")))?;
						let sessions = object
							.as_object()
							.ok_or_else(|| {
								error(500, "sessions value is not an object".to_owned())
							})?
							.keys()
							.collect::<Vec<_>>();
						if !sessions.contains(&&segments[2]) {
							return Err(error(409, "invalid session".to_owned()));
						}

						if !matches!(
							block_on(APPLICATIONS_TX.lock()).get(&segments[1]),
							Some(None)
						) || !matches!(
							block_on(APPLICATIONS_RX.lock()).get(&segments[1]),
							Some(None)
						) {
							return Err(error(502, "plugin is not connected".to_owned()));
						}
					}
					_ => return Err(error(400, "invalid connection type".to_owned())),
				}
			}

			#[cfg(feature = "self-host")]
			match &segments[0][..] {
				"plugin" => {}
				"application" => {
					if !matches!(
						block_on(APPLICATIONS_TX.lock()).get(&segments[1]),
						Some(None)
					) || !matches!(
						block_on(APPLICATIONS_RX.lock()).get(&segments[1]),
						Some(None)
					) {
						return Err(error(502, "plugin is not connected".to_owned()));
					}
				}
				_ => return Err(error(400, "invalid connection type".to_owned())),
			}

			details = Some((segments[0].clone(), segments[1].clone()));
			Ok(res)
		};

		match accept_hdr_async(stream, callback).await {
			Ok(ws_stream) => {
				let Some(details) = details else {
					continue;
				};
				handle_connection(ws_stream, details.0, details.1);
			}
			Err(error) => {
				if let Error::Http(res) = error {
					if res.status() == 502 {
						continue;
					}
					eprintln!(
						"failed to accept connection: {}: {}",
						res.status(),
						String::from_utf8_lossy(&res.body().clone().unwrap_or(vec![]))
					);
				} else {
					eprintln!("failed to accept connection: {error}");
				}
			}
		}
	}
}
