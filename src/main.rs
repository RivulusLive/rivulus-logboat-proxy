use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use futures_lite::FutureExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::spawn;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{Error, Message};
use tokio_tungstenite::{accept_hdr_async, WebSocketStream};

type Rx = SplitStream<WebSocketStream<TcpStream>>;
type Tx = SplitSink<WebSocketStream<TcpStream>, Message>;

#[derive(Debug)]
enum PluginState {
	Ready,
	Pairing(Option<Rx>, Option<Tx>),
	Occupied,
}
static PLUGIN_STATES: LazyLock<RwLock<HashMap<String, PluginState>>> =
	LazyLock::new(|| RwLock::new(HashMap::new()));

#[cfg(feature = "stripe")]
static STRIPE: LazyLock<stripe::Client> =
	LazyLock::new(|| stripe::Client::new(std::env::var("STRIPE_SECRET_KEY").unwrap()));

async fn connection_details(path: Option<String>) -> Result<(String, String), (u16, &'static str)> {
	let Some(path) = path else {
		return Err((401, "invalid credentials"));
	};
	let segments = path
		.split('/')
		.filter(|v| !v.is_empty())
		.map(|v| v.to_owned())
		.collect::<Vec<_>>();
	if segments.len() < 3 {
		return Err((401, "invalid credentials"));
	}

	#[cfg(feature = "stripe")]
	{
		use std::str::FromStr;
		let customer_id =
			stripe::CustomerId::from_str(&segments[1]).map_err(|_| (401, "invalid credentials"))?;
		let customer = stripe::Customer::retrieve(&STRIPE, &customer_id, &[])
			.await
			.map_err(|_| (401, "invalid credentials"))?;
		let metadata = customer.metadata.ok_or((500, "internal server error"))?;

		let object_key = match &segments[0][..] {
			"plugin" => "pluginSessions",
			"application" => "sessions",
			_ => return Err((401, "invalid credentials")),
		};

		let object = serde_json::from_str::<serde_json::Value>(
			metadata
				.get(object_key)
				.ok_or((500, "internal server error"))?,
		)
		.map_err(|_| (500, "internal server error"))?;
		let sessions = object
			.as_object()
			.ok_or((500, "internal server error"))?
			.keys()
			.collect::<Vec<_>>();
		if !sessions.contains(&&segments[2]) {
			return Err((401, "invalid credentials"));
		}
	}

	#[cfg(not(feature = "stripe"))]
	if !matches!(&segments[0][..], "plugin" | "application") {
		return Err((401, "invalid credentials"));
	}

	let lock = PLUGIN_STATES.read().await;
	let plugin_state = lock.get(&segments[1]);
	if segments[0] == "plugin" && plugin_state.is_some() {
		return Err((409, "plugin is already connected"));
	} else if segments[0] == "application" {
		if plugin_state.is_none() {
			return Err((502, "plugin is not connected"));
		} else if !matches!(*plugin_state.unwrap(), PluginState::Ready) {
			return Err((409, "application is already connected"));
		}
	}

	Ok((segments[0].clone(), segments[1].clone()))
}

async fn handle_connection(ws_stream: WebSocketStream<TcpStream>, from: String, id: String) {
	match &from[..] {
		"plugin" => {
			let (mut ptx, mut prx) = ws_stream.split();

			PLUGIN_STATES
				.write()
				.await
				.insert(id.clone(), PluginState::Ready);
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
							}
						};
						continue;
					};
					if let Some(catx) = &mut atx {
						let _ = catx.send(message).await;
					} else {
						let mut lock = PLUGIN_STATES.write().await;
						if let Some(PluginState::Pairing(rx, Some(mut tx))) = lock.remove(&id) {
							if rx.is_some() {
								lock.insert(id.clone(), PluginState::Pairing(rx, None));
							} else {
								lock.insert(id.clone(), PluginState::Occupied);
							}
							drop(lock);
							let _ = tx.send(message).await;
							atx = Some(tx);
						}
					}
				}
				*should_close_plugin.write().await = true;
				PLUGIN_STATES.write().await.remove(&id);
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
							let _ = ptx.send(message).await;
						}
						arx = None;
						*should_close_application_2.write().await = true;
						PLUGIN_STATES
							.write()
							.await
							.insert(id_2.clone(), PluginState::Ready);
						let _ = ptx.send(Message::Text("close".into())).await;
					} else {
						let mut lock = PLUGIN_STATES.write().await;
						if let Some(state) = lock.remove(&id_2) {
							if let PluginState::Pairing(Some(rx), tx) = state {
								if tx.is_some() {
									lock.insert(id_2.clone(), PluginState::Pairing(None, tx));
								} else {
									lock.insert(id_2.clone(), PluginState::Occupied);
								}
								arx = Some(rx);
							} else {
								lock.insert(id_2.clone(), state);
								drop(lock);
								sleep(Duration::from_millis(5)).await;
							}
						} else {
							break 'outer;
						}
					}
				}
			});
		}
		"application" => {
			let mut lock = PLUGIN_STATES.write().await;
			if let Some(PluginState::Ready) = lock.get(&id) {
				let (atx, arx) = ws_stream.split();
				lock.insert(id.clone(), PluginState::Pairing(Some(arx), Some(atx)));
			}
		}
		_ => panic!("invalid connection type"),
	};
}

#[tokio::main]
async fn main() {
	let listener = TcpListener::bind("0.0.0.0:8402").await.unwrap();

	while let Ok((stream, _)) = listener.accept().await {
		let mut path = None;
		match accept_hdr_async(stream, |req: &Request, res: Response| {
			path = Some(req.uri().path().to_string());
			Ok(res)
		})
		.await
		{
			Ok(mut ws_stream) => {
				match connection_details(path).await {
					Ok(details) => {
						spawn(handle_connection(ws_stream, details.0, details.1));
					}
					Err(error) => {
						spawn(async move {
							ws_stream
								.send(Message::Close(Some(CloseFrame {
									code: (4000 + error.0).into(),
									reason: error.1.into(),
								})))
								.await
						});
					}
				};
			}
			Err(error) => {
				eprintln!("failed to accept connection: {error}");
			}
		}
	}
}
