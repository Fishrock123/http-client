//! http-client implementation for async-h1, with connecton pooling ("Keep-Alive").

use std::net::SocketAddr;
use std::{fmt::Debug, sync::Arc};

use async_h1::client;
use async_std::net::TcpStream;
use dashmap::DashMap;
use deadpool::managed::Pool;
use http_types::StatusCode;

#[cfg(not(feature = "h1_client_rustls"))]
use async_native_tls::TlsStream;
#[cfg(feature = "h1_client_rustls")]
use async_tls::client::TlsStream;

use super::{async_trait, Error, HttpClient, Request, Response};

mod tcp;
mod tls;

use tcp::{TcpConnWrapper, TcpConnection};
use tls::{TlsConnWrapper, TlsConnection};

// TODO: Move this to a parameter. This current number is based on a few
// random benchmarks and see whatever gave decent perf vs resource use.
static MAX_CONCURRENT_CONNECTIONS: usize = 50;

type HttpPool = DashMap<SocketAddr, Pool<TcpStream, std::io::Error>>;
type HttpsPool = DashMap<SocketAddr, Pool<TlsStream<TcpStream>, Error>>;

/// Async-h1 based HTTP Client, with connecton pooling ("Keep-Alive").
pub struct H1Client {
    http_pools: Arc<HttpPool>,
    https_pools: Arc<HttpsPool>,
}

impl Debug for H1Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("H1Client")
    }
}

impl Default for H1Client {
    fn default() -> Self {
        Self::new()
    }
}

impl H1Client {
    /// Create a new instance.
    pub fn new() -> Self {
        Self {
            http_pools: Arc::new(DashMap::new()),
            https_pools: Arc::new(DashMap::new()),
        }
    }
}

#[async_trait]
impl HttpClient for H1Client {
    async fn send(&self, mut req: Request) -> Result<Response, Error> {
        let http_pools = self.http_pools.clone();
        let https_pools = self.https_pools.clone();
        req.insert_header("Connection", "keep-alive");

        // Insert host
        let host = req
            .url()
            .host_str()
            .ok_or_else(|| Error::from_str(StatusCode::BadRequest, "missing hostname"))?
            .to_string();

        let scheme = req.url().scheme();
        if scheme != "http" && scheme != "https" {
            return Err(Error::from_str(
                StatusCode::BadRequest,
                format!("invalid url scheme '{}'", scheme),
            ));
        }

        let addr = req
            .url()
            .socket_addrs(|| match req.url().scheme() {
                "http" => Some(80),
                "https" => Some(443),
                _ => None,
            })?
            .into_iter()
            .next()
            .ok_or_else(|| Error::from_str(StatusCode::BadRequest, "missing valid address"))?;

        log::trace!("> Scheme: {}", scheme);

        match scheme {
            "http" => {
                let pool = if let Some(pool) = http_pools.get(&addr) {
                    pool
                } else {
                    let manager = TcpConnection::new(addr);
                    let pool =
                        Pool::<TcpStream, std::io::Error>::new(manager, MAX_CONCURRENT_CONNECTIONS);
                    http_pools.insert(addr, pool);
                    http_pools.get(&addr).unwrap()
                };
                let pool = pool.clone();
                let stream = pool.get().await?;
                req.set_peer_addr(stream.peer_addr().ok());
                req.set_local_addr(stream.local_addr().ok());
                client::connect(TcpConnWrapper::new(stream), req).await

                // let stream = async_std::net::TcpStream::connect(addr).await?;
                // req.set_peer_addr(stream.peer_addr().ok());
                // req.set_local_addr(stream.local_addr().ok());
                // client::connect(stream, req).await
            }
            "https" => {
                let pool = if let Some(pool) = https_pools.get(&addr) {
                    pool
                } else {
                    let manager = TlsConnection::new(host.clone(), addr);
                    let pool = Pool::<TlsStream<TcpStream>, Error>::new(
                        manager,
                        MAX_CONCURRENT_CONNECTIONS,
                    );
                    https_pools.insert(addr, pool);
                    https_pools.get(&addr).unwrap()
                };
                let pool = pool.clone();
                let stream = pool.get().await.unwrap(); // TODO: remove unwrap
                req.set_peer_addr(stream.get_ref().peer_addr().ok());
                req.set_local_addr(stream.get_ref().local_addr().ok());

                client::connect(TlsConnWrapper::new(stream), req).await

                // let raw_stream = async_std::net::TcpStream::connect(addr).await?;
                // req.set_peer_addr(raw_stream.peer_addr().ok());
                // req.set_local_addr(raw_stream.local_addr().ok());

                // let stream = async_native_tls::connect(host, raw_stream).await?;

                // client::connect(stream, req).await
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_std::prelude::*;
    use async_std::task;
    use http_types::url::Url;
    use http_types::Result;
    use std::time::Duration;

    fn build_test_request(url: Url) -> Request {
        let mut req = Request::new(http_types::Method::Post, url);
        req.set_body("hello");
        req.append_header("test", "value");
        req
    }

    #[async_std::test]
    async fn basic_functionality() -> Result<()> {
        let port = portpicker::pick_unused_port().unwrap();
        let mut app = tide::new();
        app.at("/").all(|mut r: tide::Request<()>| async move {
            let mut response = tide::Response::new(http_types::StatusCode::Ok);
            response.set_body(r.body_bytes().await.unwrap());
            Ok(response)
        });

        let server = task::spawn(async move {
            app.listen(("localhost", port)).await?;
            Result::Ok(())
        });

        let client = task::spawn(async move {
            task::sleep(Duration::from_millis(100)).await;
            let request =
                build_test_request(Url::parse(&format!("http://localhost:{}/", port)).unwrap());
            let mut response: Response = H1Client::new().send(request).await?;
            assert_eq!(response.body_string().await.unwrap(), "hello");
            Ok(())
        });

        server.race(client).await?;

        Ok(())
    }

    #[async_std::test]
    async fn https_functionality() -> Result<()> {
        task::sleep(Duration::from_millis(100)).await;
        // Send a POST request to https://httpbin.org/post
        // The result should be a JSon string similar to what you get with:
        //  curl -X POST "https://httpbin.org/post" -H "accept: application/json" -H "Content-Type: text/plain;charset=utf-8" -d "hello"
        let request = build_test_request(Url::parse("https://httpbin.org/post").unwrap());
        let mut response: Response = H1Client::new().send(request).await?;
        let json_val: serde_json::value::Value =
            serde_json::from_str(&response.body_string().await.unwrap())?;
        assert_eq!(*json_val.get("data").unwrap(), serde_json::json!("hello"));
        Ok(())
    }
}