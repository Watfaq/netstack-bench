use std::{net::SocketAddr, sync::Arc};

use futures::{SinkExt, StreamExt};
use netstack_smoltcp::{StackBuilder, TcpListener, UdpSocket};
use structopt::StructOpt;
use tokio::net::{TcpSocket, TcpStream};
use tracing::{error, info, warn};
use tun_rs::{DeviceBuilder, GROTable, IDEAL_BATCH_SIZE, VIRTIO_NET_HDR_LEN};

// to run this example, you should set the policy routing **after the start of the main program**
//
// linux:
// with bind device:
// `curl 1.1.1.1 --interface utun8`
// with default route:
// `bash scripts/route-linux.sh add`
// `curl 1.1.1.1`
// with single route:
// `ip rule add to 1.1.1.1 table 200`
// `ip route add default dev utun8 table 200`
// `curl 1.1.1.1`
//
// macos:
// with default route:
// `bash scripts/route-macos.sh add`
// `curl 1.1.1.1`
//
// windows:
// with default route:
// tun2 set default route automatically, won't set agian
// # `powershell.exe scripts/route-windows.ps1 add`
// `curl 1.1.1.1`
//
// currently, the example only supports the TCP stream, and the UDP packet will be dropped.

#[derive(Debug, StructOpt)]
#[structopt(name = "forward", about = "Simply forward tun tcp/udp traffic.")]
struct Opt {
    /// Default binding interface, default by guessed.
    /// Specify but doesn't exist, no device is bound.
    #[structopt(short = "i", long = "interface")]
    interface: String,

    /// name of the tun device, default to rtun8.
    #[structopt(short = "n", long = "name", default_value = "utun8")]
    name: String,

    /// Tracing subscriber log level.
    #[structopt(long = "log-level", default_value = "debug")]
    log_level: tracing::Level,

    /// Tokio current-thread runtime, default to multi-thread.
    #[structopt(long = "current-thread")]
    current_thread: bool,

    /// Tokio task spawn_local, default to spwan.
    #[structopt(long = "local-task")]
    local_task: bool,
}

fn main() {
    let opt = Opt::from_args();

    let rt = if opt.current_thread {
        tokio::runtime::Builder::new_current_thread()
    } else {
        tokio::runtime::Builder::new_multi_thread()
    }
    .enable_all()
    .build()
    .unwrap();

    rt.block_on(main_exec(opt));
}

async fn main_exec(opt: Opt) {
    macro_rules! tokio_spawn {
        ($fut: expr) => {
            if opt.local_task {
                tokio::task::spawn_local($fut)
            } else {
                tokio::task::spawn($fut)
            }
        };
    }

    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_max_level(opt.log_level)
            .finish(),
    )
    .unwrap();

    let dev = DeviceBuilder::new()
        .layer(tun_rs::Layer::L3)
        .name(&opt.name)
        .ipv4("10.10.10.2", 24, Some("10.10.10.1"))
        .mtu(1400)
        .offload(true)
        .build_async()
        .unwrap();
    let builder = StackBuilder::default().enable_tcp(true).enable_udp(true);

    let dev = Arc::new(dev);
    let dev1 = dev.clone();

    let (stack, runner, udp_socket, tcp_listener) = builder.build().unwrap();
    let udp_socket = udp_socket.unwrap(); // udp enabled
    let tcp_listener = tcp_listener.unwrap(); // tcp enabled or icmp enabled

    if let Some(runner) = runner {
        tokio_spawn!(runner);
    }

    let (mut stack_sink, mut stack_stream) = stack.split();

    let mut futs = vec![];

    // Reads packet from stack and sends to TUN.
    futs.push(tokio_spawn!(async move {
        while let Some(pkt) = stack_stream.next().await {
            let mut gro_table = GROTable::default();
            if let Ok(pkt) = pkt {
                // TODO: could we introduce friendlier `send_multiple` method
                // for not building the pkt again? or shall we reserve the space for hdr in our netstack impl?
                let mut pkt_with_hdr = Vec::with_capacity(VIRTIO_NET_HDR_LEN + pkt.len());
                pkt_with_hdr.extend_from_slice(&[0; VIRTIO_NET_HDR_LEN]);
                pkt_with_hdr.extend_from_slice(&pkt);
                let mut bufs = vec![pkt_with_hdr];
                match dev
                    .send_multiple(&mut gro_table, &mut bufs, VIRTIO_NET_HDR_LEN)
                    .await
                {
                    Ok(_) => {}
                    Err(e) => warn!("failed to send packet to TUN, err: {:?}", e),
                }
            }
        }
    }));

    // Reads packet from TUN and sends to stack.
    futs.push(tokio_spawn!(async move {
        let mut original_buffer = vec![0; VIRTIO_NET_HDR_LEN + 65535];
        let mut bufs = vec![vec![0u8; 1500]; IDEAL_BATCH_SIZE];
        let mut sizes = vec![0; IDEAL_BATCH_SIZE];

        while let Ok(num) = dev1
            .recv_multiple(&mut original_buffer, &mut bufs, &mut sizes, 0)
            .await
        {
            for i in 0..num {
                if let Err(err) = stack_sink.send(bufs[i][..sizes[i]].to_vec()).await {
                    warn!("failed to send packet to stack, err: {:?}", err);
                }
            }
        }
    }));

    // Extracts TCP connections from stack and sends them to the dispatcher.
    futs.push(tokio_spawn!({
        let interface = opt.interface.clone();
        async move {
            handle_inbound_stream(tcp_listener, interface).await;
        }
    }));

    // Receive and send UDP packets between netstack and NAT manager. The NAT
    // manager would maintain UDP sessions and send them to the dispatcher.
    futs.push(tokio_spawn!(async move {
        handle_inbound_datagram(udp_socket, opt.interface).await;
    }));

    futures::future::join_all(futs)
        .await
        .iter()
        .for_each(|res| {
            if let Err(e) = res {
                error!("error: {:?}", e);
            }
        });
}

/// simply forward tcp stream
async fn handle_inbound_stream(mut tcp_listener: TcpListener, interface: String) {
    while let Some((mut stream, local, remote)) = tcp_listener.next().await {
        let interface = interface.clone();
        tokio::spawn(async move {
            println!("new tcp connection: {:?} => {:?}", local, remote);
            match new_tcp_stream(remote, &interface).await {
                Ok(mut remote_stream) => {
                    // pipe between two tcp stream
                    match tokio::io::copy_bidirectional(&mut stream, &mut remote_stream).await {
                        Ok(_) => {}
                        Err(e) => warn!(
                            "failed to copy tcp stream {:?}=>{:?}, err: {:?}",
                            local, remote, e
                        ),
                    }
                }
                Err(e) => warn!(
                    "failed to new tcp stream {:?}=>{:?}, err: {:?}",
                    local, remote, e
                ),
            }
        });
    }
}

/// simply forward udp datagram
async fn handle_inbound_datagram(udp_socket: UdpSocket, interface: String) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (mut read_half, mut write_half) = udp_socket.split();
    tokio::spawn(async move {
        while let Some((data, local, remote)) = rx.recv().await {
            let _ = write_half.send((data, remote, local)).await;
        }
    });

    while let Some((data, local, remote)) = read_half.next().await {
        let tx = tx.clone();
        let interface = interface.clone();
        tokio::spawn(async move {
            info!("new udp datagram: {:?} => {:?}", local, remote);
            match new_udp_packet(remote, &interface).await {
                Ok(remote_socket) => {
                    // pipe between two udp sockets
                    let _ = remote_socket.send(&data).await;
                    loop {
                        let mut buf = vec![0; 1024];
                        match remote_socket.recv_from(&mut buf).await {
                            Ok((len, _)) => {
                                let _ = tx.send((buf[..len].to_vec(), local, remote));
                            }
                            Err(e) => {
                                warn!(
                                    "failed to recv udp datagram {:?}<->{:?}: {:?}",
                                    local, remote, e
                                );
                                break;
                            }
                        }
                    }
                }
                Err(e) => warn!(
                    "failed to new udp socket {:?}=>{:?}, err: {:?}",
                    local, remote, e
                ),
            }
        });
    }
}

async fn new_tcp_stream<'a>(addr: SocketAddr, iface: &str) -> std::io::Result<TcpStream> {
    use socket2_ext::{AddressBinding, BindDeviceOption};
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)?;
    socket.bind_to_device(BindDeviceOption::v4(iface))?;
    socket.set_keepalive(true)?;
    socket.set_nodelay(true)?;
    socket.set_nonblocking(true)?;

    let stream = TcpSocket::from_std_stream(socket.into())
        .connect(addr)
        .await?;

    Ok(stream)
}

async fn new_udp_packet(addr: SocketAddr, iface: &str) -> std::io::Result<tokio::net::UdpSocket> {
    use socket2_ext::{AddressBinding, BindDeviceOption};
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None)?;
    socket.bind_to_device(BindDeviceOption::v4(iface))?;
    socket.set_nonblocking(true)?;

    let socket = tokio::net::UdpSocket::from_std(socket.into());
    if let Ok(ref socket) = socket {
        socket.connect(addr).await?;
    }
    socket
}
