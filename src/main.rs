#![feature(pin)]
#![feature(await_macro, async_await, futures_api)]
#![feature(try_blocks)]
#![feature(custom_attribute)]

#[macro_use] extern crate tokio;
extern crate futures;
extern crate tokio_process;
extern crate bytes;
#[macro_use] extern crate log;
extern crate env_logger;
extern crate base64;
extern crate structopt;
extern crate serde;
#[macro_use] extern crate serde_derive;
extern crate serde_ini;
extern crate clap;
extern crate regex;

extern crate stun3489;
extern crate opendht;

macro_rules! log_err {
    ($expr: expr, $msg: expr) => ({
        if let Err(err) = $expr {
            error!($msg, err);
        }
    })
}

use std::io::Error;
use std::net::SocketAddr;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::Mutex;
use std::collections::HashMap;

use tokio::prelude::*;

use structopt::StructOpt;

mod wg;
mod dht;
mod stun;
mod utils;
mod traffic;
mod dht_encoding;

use crate::utils::CloneReceiver;
use crate::utils::CloneSender;

#[derive(Debug)]
struct CmdOpts {
    verbose: bool,
    peers: HashMap<String, Vec<Vec<u8>>>,
    stun_server: String,
}

impl CmdOpts {
    fn parse() -> CmdOpts {
        let matches = clap::App::new("wg-p2p")
                        .version("0.0")
                        .about("A peer-to-peer daemon for wireguard.")
                        .arg(clap::Arg::with_name("verbose")
                            .short("v")
                            .long("verbose")
                        )
                        .arg(clap::Arg::with_name("iface")
                            .short("i")
                            .long("iface")
                            .value_name("NAME")
                            .takes_value(true)
                            .required(true)
                        )
                        .arg(clap::Arg::with_name("peer")
                            .short("p")
                            .long("peer")
                            .value_name("PUBKEY")
                            .takes_value(true)
                            .required(true)
                        )
                        .get_matches();
        let mut iface_indices = matches.indices_of("NAME").unwrap();
        let mut iface_names = matches.values_of("NAME").unwrap();

        let mut peers = HashMap::new();

        let mut start = iface_indices.next().unwrap();
        for end in iface_indices {
            let indices = matches.indices_of("PUBKEY").unwrap();
            let pubkeys = matches.values_of("PUBKEY").unwrap();

            let keys = indices.zip(pubkeys).filter_map(|(i, key)|
                if start < i && i < end {
                    Some(base64::decode(&key).unwrap())
                } else {
                    None
                }).collect();

            peers.insert(iface_names.next().unwrap().to_string(), keys);
            start = end;
        }

        let indices = matches.indices_of("PUBKEY").unwrap();
        let pubkeys = matches.values_of("PUBKEY").unwrap();
        let keys = indices.zip(pubkeys).filter_map(|(i, key)|
            if start < i {
                Some(base64::decode(&key).unwrap())
            } else {
                None
            }).collect();

        peers.insert(iface_names.next().unwrap().to_string(), keys);

        CmdOpts {
            peers,
            verbose: matches.is_present("verbose"),
            stun_server: matches.value_of("stun-server").unwrap_or("stun.wtfismyip.com:3478").to_string(),
        }
    }
}

#[derive(Debug, StructOpt)]
#[structopt(name = "wg-p2p", about = "A peer-to-peer daemon for wireguard.")]
struct CmdOpt {
    #[structopt(short = "v", long = "verbose")]
    verbose: bool,

    #[structopt(short = "i", long = "iface")]
    iface: String,

    #[structopt(short = "n", long = "netns")]
    netns: Option<String>,

    #[structopt(short = "p", long = "peer")]
    peer: Option<String>,

    #[structopt(long = "stun", default_value = "stun.wtfismyip.com:3478")]
    stun_server: String,

    #[structopt(long = "dht-port", default_value = "4222")]
    dht_port: u16,
}

fn inject<T>(mut stream: impl Stream<Item=T, Error=impl std::fmt::Debug + Send> + std::marker::Unpin + Send + 'static)
    -> (impl Sink<SinkItem=T, SinkError=impl std::fmt::Debug> + Clone, impl Stream<Item=T, Error=impl std::fmt::Debug>)
    where T: Send + 'static
{
    let (tx, rx) = futures::sync::mpsc::unbounded();
    let mut ttx = tx.clone();
    tokio::spawn_async(async move {
        while let Some(res) = await!(stream.next()) {
            match res {
                Ok(data) => { await!(ttx.send_async(data)).unwrap(); },
                Err(err) => warn!("{:?}", err),
            }
        }
    });

    (tx, rx)
}

fn main() -> Result<(), Error> {
    let opt = CmdOpt::from_args();

    if opt.verbose {
        log::set_max_level(log::LevelFilter::Debug);
    }

    env_logger::init();

    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0);

    let bind_addr = SocketAddr::from(([0, 0, 0, 0], 0));
    let stun_server = opt.stun_server;

    let netns = opt.netns;
    let dht_port = opt.dht_port;
    tokio::run_async(async move {
        let dht = await!(dht::Dht::new(dht_port));

        for wg_iface in await!(wg::get_interfaces(netns.clone())).unwrap().into_iter() {
            let public_addr = Arc::new(Mutex::new(None)); // TODO: use stream instead of mutex

            let sock = tokio::net::UdpSocket::bind(&addr).unwrap();
            let codec = tokio::codec::BytesCodec::new();
            let (udp_tx, udp_rx) = tokio::net::UdpFramed::new(sock, codec).split();

            let wg_cfg = await!(wg::get_config(netns.clone(), &wg_iface)).unwrap();
            let wg_port = wg_cfg.interface.listen_port;
            debug!("Wireguard Port {}", wg_port);

            let (inet2stun_tx, inet2stun_rx) = futures::sync::mpsc::unbounded();
            let (udp_tx, stun2inet_tx) = udp_tx.clone_sink();

            let (new_endpoints_tx, mut new_endpoints_rx) = futures::sync::mpsc::unbounded();

            let (dht2wg_tx, udp_rx) = inject(udp_rx);
//            tokio::spawn_async(traffic::forward_outbound(inbound_rx, udp_tx1));
            tokio::spawn_async(traffic::forward_inbound(new_endpoints_tx, udp_rx, inet2stun_tx, udp_tx, wg_port));
            tokio::spawn_async(stun::run(inet2stun_rx, stun2inet_tx, bind_addr, stun_server.clone(), public_addr.clone()));

            let local_public_key = await!(wg::local_public_key(netns.clone(), &wg_iface)).unwrap();

            let peer_list = wg_cfg.peers.iter().map(|p| &p.public_key);
            for remote_public_key in peer_list {
                info!("Managing peer {} on interface {}.", remote_public_key, wg_iface.clone());
                let remote_public_key = base64::decode(remote_public_key).unwrap();
                let public_addr2 = public_addr.clone();

                let (rx, new_endpoints_rrx) = new_endpoints_rx.clone_stream();
                new_endpoints_rx = rx;
                
                let dht2 = dht.clone();
                let local_public_key2 = local_public_key.clone();
                let remote_public_key2 = remote_public_key.clone();
                tokio::spawn_async(async move {
                    await!(dht2.put_loop(public_addr2, local_public_key2, remote_public_key2))
                });

                let dht2 = dht.clone();
                let dht2wg_ttx = dht2wg_tx.clone();
                let local_public_key2 = local_public_key.clone();
                let remote_public_key2 = remote_public_key.clone();
                let wg_iface = wg_iface.clone();
                let netns = netns.clone();
                tokio::spawn_async(async move {
                    await!(dht2.get_loop(netns.clone(), new_endpoints_rrx, local_public_key2, remote_public_key2, &wg_iface, dht2wg_ttx));
                });
            }

            tokio::spawn_async(async move {
                while let Some(_) = await!(new_endpoints_rx.next()) {
                    // noop
                }
            });
        }
    });

    Ok(())
}
