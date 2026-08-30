//! Small HTTP/1.1 fixtures for transport-level API tests.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};

#[derive(Debug)]
pub(crate) struct ObservedRequest {
    pub request_line: String,
    pub authorization: Option<String>,
    pub body: Vec<u8>,
}

pub(crate) fn read_request(stream: &std::net::TcpStream) -> ObservedRequest {
    let mut reader = BufReader::new(stream.try_clone().expect("clone test connection"));
    let mut line = String::new();
    reader.read_line(&mut line).expect("read request line");
    let request_line = line.trim_end_matches(['\r', '\n']).to_owned();
    let mut authorization = None;
    let mut content_length = 0;
    loop {
        line.clear();
        reader.read_line(&mut line).expect("read request header");
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().expect("numeric content length");
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).expect("read request body");
    ObservedRequest {
        request_line,
        authorization,
        body,
    }
}

pub(crate) fn write_response(
    mut stream: std::net::TcpStream,
    status: &str,
    extra_headers: &[(&str, &str)],
    body: &str,
) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    )
    .expect("write response headers");
    for (name, value) in extra_headers {
        write!(stream, "{name}: {value}\r\n").expect("write response header");
    }
    write!(stream, "\r\n{body}").expect("write response body");
}

pub(crate) struct DelayedResponses {
    pub port: u16,
    first_seen: tokio::sync::oneshot::Receiver<()>,
    release: std::sync::mpsc::Sender<()>,
    observed: tokio::sync::oneshot::Receiver<(Vec<ObservedRequest>, usize)>,
}

impl DelayedResponses {
    pub fn start(expected: usize) -> Self {
        use std::net::{Ipv4Addr, TcpListener};

        assert!(expected > 0);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("loopback API");
        let port = listener.local_addr().expect("API address").port();
        let (first_seen_tx, first_seen) = tokio::sync::oneshot::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let (observed_tx, observed) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let (first_stream, _) = listener.accept().expect("first API request");
            let first = read_request(&first_stream);
            let _ = first_seen_tx.send(());
            let mut pending = vec![(first_stream, first)];
            listener.set_nonblocking(true).expect("nonblocking probe");
            loop {
                if release_rx.try_recv().is_ok() {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        let request = read_request(&stream);
                        pending.push((stream, request));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    Err(error) => panic!("probe for an early API request: {error}"),
                }
            }
            let arrived_early = pending.len().saturating_sub(1);
            let mut requests = Vec::new();
            for (stream, request) in pending {
                write_response(stream, "200 OK", &[], "{}");
                requests.push(request);
            }
            listener.set_nonblocking(false).expect("blocking accept");
            while requests.len() < expected {
                let (stream, _) = listener.accept().expect("remaining API request");
                let request = read_request(&stream);
                write_response(stream, "200 OK", &[], "{}");
                requests.push(request);
            }
            let _ = observed_tx.send((requests, arrived_early));
        });
        Self {
            port,
            first_seen,
            release,
            observed,
        }
    }

    pub async fn observe(mut self) -> (Vec<ObservedRequest>, usize) {
        (&mut self.first_seen)
            .await
            .expect("first API request arrives");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        self.release.send(()).expect("release first API response");
        self.observed.await.expect("API observer exits")
    }
}
