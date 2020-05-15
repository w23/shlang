use {
    std::{
        os::unix::io::AsRawFd,
        net::{SocketAddr, ToSocketAddrs},
    },
    mio::{
        Events, Poll, Token, Interest,
        net::{
            UdpSocket,
        },
        unix::SourceFd,
    },
    log::{info, trace, warn, error, debug},
};

#[derive(Debug)]
struct Args {
    listen: bool,
    addr: SocketAddr, // TODO multiple variants
}

impl Args {
    fn read() -> Result<Args, Box<dyn std::error::Error>> {
        let mut args = pico_args::Arguments::from_env();
        let listen = args.contains("-l");
        let addr = args.free()?[0].to_socket_addrs()?.next().unwrap();
        Ok(Args{
            listen,
            addr,
        })
    }
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let mut poll = Poll::new()?;
    let mut socket = if args.listen {
        UdpSocket::bind(args.addr)?
    } else {
        let socket = UdpSocket::bind("127.0.0.1:0".parse()?)?; // TODO ipv6
        socket.connect(args.addr)?;
        socket
    };

    const STDIN: Token = Token(0);
    const STDOUT: Token = Token(1);
    const SOCKET: Token = Token(2);

    let stdin = std::io::stdin().as_raw_fd();
    let mut stdin = SourceFd(&stdin);
    let stdout = std::io::stdout().as_raw_fd();
    let mut stdout = SourceFd(&stdout);

    poll.registry().register(&mut socket, SOCKET, Interest::WRITABLE | Interest::READABLE)?;
    poll.registry().register(&mut stdin, STDIN, Interest::READABLE)?;
    poll.registry().register(&mut stdout, STDOUT, Interest::WRITABLE)?;

    let mut events = Events::with_capacity(16);
    'outer: loop {
        match poll.poll(&mut events, None) {
            Ok(_) => {},
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                continue; },
            Err(e) => {
                return Err(Box::new(e));
            }
        }

        for event in events.iter() {
            debug!("{:?}", event);
            if event.is_read_closed() {
                break 'outer;
            }
        }
    }

    Ok(())
}

fn main() {
    stderrlog::new().module(module_path!()).verbosity(6).init().unwrap();

    let args = match Args::read() {
        Ok(args) => args,
        Err(e) => {
            error!("Error parsing args: {}", e);
            return;
        }
    };

    println!("{:?}", args);

    match run(&args) {
        Ok(_) => {},
        Err(e) => {
            error!("Runtime error: {}", e);
        }
    }
}
