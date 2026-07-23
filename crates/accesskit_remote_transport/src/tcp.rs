//! Localhost TCP, the development fallback transport.

use std::io;
use std::net::{TcpListener, TcpStream};

pub fn listen_local(port: u16) -> io::Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", port))
}

pub fn connect_local(port: u16) -> io::Result<TcpStream> {
    TcpStream::connect(("127.0.0.1", port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn round_trip() {
        let listener = listen_local(0).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            stream.write_all(&buf).unwrap();
        });
        let mut client = connect_local(port).unwrap();
        client.write_all(b"hello").unwrap();
        let mut buf = [0u8; 5];
        client.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        server.join().unwrap();
    }
}
