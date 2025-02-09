## Rivulus Logboat WebSocket proxy

As the Logboat OpenAction plugin runs on the user's computer, it cannot expose a secure WebSocket server without using a self-signed certificate, and since the application is served over HTTPS, the browser enforces that only secure WebSocket servers can be connected to.

Therefore, both the plugin and the application connect to this proxy, using both the user ID and authenticated using their respective session tokens to prevent bad actors interfering with other users' applications.

This proxy is open source to allow those who are concerned about privacy to audit the code, and to allow those who are concerned about privacy and have the capabilities to self-host the proxy to do so.

#### Self-hosting

Clone this repository and compile the proxy executable with the `self-host` feature (note: this disables all authentication functionality, so make sure your proxy address is not known to anyone else):

`cargo build --release --features "self-host"`

Run the produced executable and expose port 8402 on your machine on an HTTPS-enabled subdomain (e.g. using Cloudflare Tunnels). Then, edit the Logboat plugin's settings file in the OpenDeck configuration directory with the `proxyDomain` key set to your proxy's domain name, without protocols or a trailing slash, and similarly set the domain name in the Logboat application's settings.
