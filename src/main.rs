mod packetizer;

use {
    std::{
        //os::unix::io::AsRawFd,
        io::{Write, Read, Cursor, Seek, IoSlice, IoSliceMut},
        net::{SocketAddr, ToSocketAddrs},
        collections::{HashMap},
    },
    mio::{
        Events, Poll, Token, Interest,
        net::{
            UdpSocket,
        },
        //unix::SourceFd,
    },
    log::{info, trace, warn, error, debug},
    packetizer::{ReadPipe, Packetizer},
};


#[derive(Debug)]
struct Args {
    listen: bool,
    addr: SocketAddr, // TODO multiple variants
    send_buffer_size: usize,
}

const DEFAULT_BUFFER_SIZE: usize = 8*1024*1024;

impl Args {
    fn read() -> Result<Args, Box<dyn std::error::Error>> {
        let mut args = pico_args::Arguments::from_env();
        let listen = args.contains("-l");
        // TODO buffer size
        let addr = match args.free()?.get(0) {
            Some(arg) => arg,
            None => return Err(Box::new(simple_error::SimpleError::new("Address was not specified"))),
        }.to_socket_addrs()?.next().unwrap();
        Ok(Args{
            listen,
            addr,
            send_buffer_size: DEFAULT_BUFFER_SIZE,
        })
    }

    fn print_usage() {
        let name = std::env::args().next().unwrap();
        println!("Usage: {} [options] addr", name);
        println!("\t-l -- listen mode");
    }
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let mut poll = Poll::new()?;
    let mut socket = if args.listen {
        UdpSocket::bind(args.addr)?
    } else {
        let socket = UdpSocket::bind("0.0.0.0:0".parse()?)?; // TODO ipv6
        socket.connect(args.addr)?;
        socket
    };

    const SOCKET: Token = Token(2);
    // const STDIN: Token = Token(0);
    // const STDOUT: Token = Token(1);

    let stdin = std::io::stdin();
    //let mut stdin_fd = SourceFd(&stdin.as_raw_fd());
    // let stdout = std::io::stdout().as_raw_fd();
    // let mut stdout = SourceFd(&stdout);
    poll.registry().register(&mut socket, SOCKET, Interest::WRITABLE | Interest::READABLE)?;
    // poll.registry().register(&mut stdin, STDIN, Interest::READABLE)?;
    // poll.registry().register(&mut stdout, STDOUT, Interest::WRITABLE)?;

    let mut source = ReadPipe::new(stdin);
    let mut to_send = Packetizer::new(DEFAULT_BUFFER_SIZE);

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

        match to_send.write_from(&mut source) {
            _ => {}
        }

        for event in events.iter() {
            trace!("{:?}", event);
            if event.is_read_closed() {
                break 'outer;
            }

            match event.token() {
                SOCKET => {
                    let mut buf = [0u8; 1400];
                    let size = to_send.generate(&mut buf).unwrap();
                    socket.send(&buf[0..size]).unwrap(); // FIXME handle errors
                },
                _ => {},
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
            Args::print_usage();
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
