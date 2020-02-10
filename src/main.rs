use std::error::Error;
use std::env;
use std::net::{SocketAddr, IpAddr};
use std::time::Duration;
use std::sync::Arc;

use byteorder::{NetworkEndian, ByteOrder};
use tokio::net::{TcpListener, TcpStream};
use tokio::prelude::*;
use derive_more::Display;
use log::{debug, info, warn, error};
use clap::{self, Arg};

mod proxy;
mod config;
mod stats;

use config::Config;

async fn handshake_auth(socket: &mut TcpStream) -> Result<bool, tokio::io::Error> {
    let mut init_buf = [0u8; 2];
    socket.read_exact(&mut init_buf).await?;
    if init_buf[0] != 0x05 {
        socket.write_all(&[0x05u8, 0xff]).await?;
        return Ok(false);
    }
    let num_auths = init_buf[1];
    if num_auths > 0xfe {
        socket.write_all(&[0x05u8, 0xff]).await?;
        return Ok(false);
    }
    let mut auths = vec![0u8; num_auths as usize];
    socket.read_exact(&mut auths).await?;
    if auths.iter().any(|&i| i == 0u8) {
        socket.write_all(&[0x5u8, 0x00u8]).await?;
        Ok(true)
    } else {
        socket.write_all(&[05u8, 0xffu8]).await?;
        Ok(false)
    }
}

#[derive(Debug, Clone)]
enum Address {
    IpAddr(IpAddr),
    DomainName(String)
}

#[derive(Debug, Clone)]
struct Request {
    address: Address,
    dport: u16,
}

impl Request {
    async fn connect(self, config: &Config) -> Result<Option<TcpStream>, tokio::io::Error> {
        let conn = match self.address {
            Address::IpAddr(i) => {
                if config.is_permitted(i, self.dport) {
                    Some(TcpStream::connect((i, self.dport)).await?)
                } else {
                    None
                }
            }
            Address::DomainName(d) => {
                for addr in tokio::net::lookup_host(d.as_str()).await? {
                    let addr = SocketAddr::new(addr.ip(), self.dport);
                    if config.is_permitted(addr.ip(), self.dport) {
                        return Ok(Some(TcpStream::connect(addr).await?));
                    }
                }
                None
            }
        };
        Ok(conn)
    }
}


#[derive(Debug, Display)]
enum RequestError {
    BadAddressType,
    IoError(tokio::io::Error),
}

impl Error for RequestError { }

impl From<tokio::io::Error> for RequestError {
    fn from(t: tokio::io::Error) -> Self {
        RequestError::IoError(t)
    }
}

#[derive(Debug)]
enum Reply {
    SocksFailure,
    ConnectionNotAllowed,
    NetworkUnreachable,
    ConnectionRefused,
    TtlExpired,
    CommandNotSupported,
    AddressNotSupported
}

impl Reply {
    fn as_u8(&self) -> u8 {
        use Reply::*;

        match self {
            SocksFailure => 0x01,
            ConnectionNotAllowed => 0x02,
            NetworkUnreachable => 0x03,
            ConnectionRefused => 0x04,
            TtlExpired => 0x05,
            CommandNotSupported => 0x07,
            AddressNotSupported => 0x08,
        }
    }

    async fn write_error<A: AsyncWrite + Unpin>(&self, into: &mut A) -> Result<(), tokio::io::Error> {
        into.write_all(&[0x05, self.as_u8(), 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]).await
    }
}

async fn copy_then_shutdown<S, D>(src: &mut S, dest: &mut D) -> Result<(), tokio::io::Error>
    where S: AsyncRead + Unpin,
          D: AsRef<TcpStream> + AsyncWrite + Unpin
{
    tokio::io::copy(src, dest).await?;
    dest.as_ref().shutdown(std::net::Shutdown::Write)?;
    Ok(())
}

async fn read_request(socket: &mut TcpStream) -> Result<Option<Request>, RequestError> {
    let mut fixed_buf = [0u8; 4];
    socket.read_exact(&mut fixed_buf).await?;
    let ver = fixed_buf[0];
    let cmd = fixed_buf[1];
    if ver != 0x05 {
        Reply::SocksFailure.write_error(socket).await?;
        return Ok(None);
    }
    if cmd != 0x01 {
        Reply::CommandNotSupported.write_error(socket).await?;
        return Ok(None);
    }
    let address = match fixed_buf[3] {
        0x01 => {
            let mut buf = [0u8; 4];
            socket.read_exact(&mut buf).await?;
            Address::IpAddr(IpAddr::V4(buf.into()))
        },
        0x03 => {
            let mut len_buf = [0u8; 1];
            socket.read_exact(&mut len_buf).await?;
            let mut name = vec![0u8; len_buf[0] as usize];
            socket.read_exact(&mut name).await?;
            Address::DomainName(String::from_utf8(name).unwrap())
        },
        0x04 => {
            let mut buf = [0u8; 16];
            socket.read_exact(&mut buf).await?;
            Address::IpAddr(IpAddr::V6(buf.into()))
        }
        _ => return Err(RequestError::BadAddressType)
    };
    let mut port_buf = [0u8; 2];
    socket.read_exact(&mut port_buf).await?;
    let dport = NetworkEndian::read_u16(&port_buf);
    Ok(Some(Request {
        address,
        dport
    }))
}

async fn handle_one_connection(mut socket: TcpStream, address: SocketAddr, config: Arc<Config>, conn_id: u64) -> Result<bool, Box<dyn Error>> {
    debug!("{}: accepted connection from {:?}", conn_id, address);
    if ! handshake_auth(&mut socket).await? {
        debug!("{}: handshake failed", conn_id);
        return Ok(false);
    }
    debug!("{}: handshake succeeded", conn_id);
    let request = read_request(&mut socket).await?;
    if let Some(request) = request {
        let mut conn = match tokio::time::timeout(
                Duration::from_millis(3000),
                request.clone().connect(&config)
            ).await {
            Ok(c) => match c {
                Ok(Some(c)) => c,
                Ok(None) => {
                    warn!("{}: denying connection to {:?}", conn_id, request);
                    Reply::ConnectionNotAllowed.write_error(&mut socket).await?;
                    return Ok(false);
                },
                Err(_) => {
                    Reply::NetworkUnreachable.write_error(&mut socket).await?;
                    return Ok(false);
                }
            }
            Err(e) => {
                warn!("{}: timeout connecting: {:?}", conn_id, e);
                Reply::TtlExpired.write_error(&mut socket).await?;
                return Ok(false);
            }
        };
        let local_end = conn.local_addr()?;
        debug!("{}: connected to {:?}", conn_id, local_end);
        socket.write_all(&[0x05, 0x00, 0x01, match local_end {
            SocketAddr::V4(_) => 0x01,
            SocketAddr::V6(_) => 0x04
        }]).await?;
        match local_end.ip() {
            IpAddr::V4(i) => socket.write_all(&i.octets()).await?,
            IpAddr::V6(i) => socket.write_all(&i.octets()).await?,
        };
        let mut buf = [0u8; 2];
        NetworkEndian::write_u16(&mut buf, local_end.port());
        socket.write_all(&buf).await?;
        let (mut conn_r, mut conn_w) = conn.split();
        let (mut socket_r,  mut socket_w) = socket.split();
        let (first, second) = tokio::join!(
            copy_then_shutdown(&mut conn_r, &mut socket_w),
            copy_then_shutdown(&mut socket_r, &mut conn_w)
        );
        first?;
        second?;
        Ok(true)
    } else {
        Ok(false)
    }
}

async fn handle_one_connection_wrapper(socket: TcpStream, address: SocketAddr, config: Arc<Config>, stats: Arc<stats::Stats>) {
    let conn_id = stats.start_request();
    match tokio::time::timeout(Duration::from_secs(300000), handle_one_connection(socket, address, config, conn_id)).await {
        Ok(Ok(_)) => (),
        Ok(Err(e)) => error!("error handling session {}: {:?}", conn_id, e),
        Err(_) => eprintln!("session {} timed out!", conn_id),
    }
    stats.finish_request(conn_id);
}


#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let matches = clap::App::new(env!("CARGO_PKG_NAME"))
                            .version(env!("CARGO_PKG_VERSION"))
                            .author("EasyPost <oss@easypost.com>")
                            .about(env!("CARGO_PKG_DESCRIPTION"))
                            .arg(Arg::with_name("config")
                                     .short("c")
                                     .long("config")
                                     .value_name("PATH")
                                     .help("Path to configuration TOML file")
                                     .takes_value(true)
                                     .required(true))
                            .get_matches();

    env_logger::init();

    let conf = Arc::new(Config::from_path(matches.value_of("config").unwrap())?);

    let stats = Arc::new(stats::Stats::new());

    let mut listener = TcpListener::bind(&conf.bind_address).await?;
    info!("Listening on: {}", conf.bind_address);

    loop {
        let (socket, address) = listener.accept().await?;

        let my_config = Arc::clone(&conf);
        let my_stats = Arc::clone(&stats);

        tokio::spawn(handle_one_connection_wrapper(socket, address, my_config, my_stats));
    }
}
