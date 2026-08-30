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
