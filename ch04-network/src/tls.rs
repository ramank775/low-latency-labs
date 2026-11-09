//! 4.8, where the decrypt runs: rustls in the reading thread, over a
//! non-blocking TCP socket, the handshake paid blocking at startup. Built only
//! with `--features tls`. The chapter's block as one function; the venue's
//! address and name are arguments, and `key_log` is Lab 4.5's switch for
//! SSLKEYLOGFILE (rustls writes the session secrets there when it is set).

use std::io::{ErrorKind, Read};
use std::net::TcpStream;
use std::sync::Arc;
use rustls::{ClientConfig, ClientConnection, RootCertStore};

pub fn tls_feed(addr: &str, server: &str, key_log: bool, mut on_bytes: impl FnMut(&[u8]))
    -> Result<(), Box<dyn std::error::Error>>
{
    let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let mut config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    if key_log { config.key_log = Arc::new(rustls::KeyLogFile::new()); } // Lab 4.5: SSLKEYLOGFILE
    let config = Arc::new(config); // kept: every reconnect reuses its session cache
    let name = server.to_owned().try_into()?;
    let mut tls = ClientConnection::new(config.clone(), name)?;
    let mut sock = TcpStream::connect(addr)?; // TCP: 1 round trip
    sock.set_nodelay(true)?;
    while tls.is_handshaking() {
        tls.complete_io(&mut sock)?; // TLS 1.3: 1 round trip, blocking, at startup
    }
    // ... the WebSocket upgrade and the subscription, still blocking ...
    sock.set_nonblocking(true)?; // from here on the thread never sleeps

    let mut plain = [0u8; 16384];
    loop {
        if tls.wants_write() { // pongs written into tls.writer(), and rustls's own records
            match tls.write_tls(&mut sock) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => {} // send buffer full: next pass
                Err(e) => return Err(e.into()),
            }
        }
        match tls.read_tls(&mut sock) { // one recv: ciphertext into rustls
            Ok(0) => return Ok(()),     // the venue closed the connection
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => continue, // poll again
            Err(e) => return Err(e.into()),
        }
        tls.process_new_packets()?; // decrypt and check every complete record
        loop {
            match tls.reader().read(&mut plain) {
                Ok(0) => return Ok(()),         // close_notify: a clean end
                Ok(n) => on_bytes(&plain[..n]), // WebSocket frames, to the framer
                Err(e) if e.kind() == ErrorKind::WouldBlock => break, // drained
                Err(e) => return Err(e.into()),
            }
        }
    }
}
