mod OchenCircusBuf;
mod connection;
mod receive;
mod sende;
mod sequence;

use {
	connection::{Connection, ConnectionParams},
	log::{/* debug, */ error, /* info, */ trace /* warn */},
	mio::{net::UdpSocket, unix::SourceFd, Events, Interest, Poll, Token},
	sende::ReadPipe,
	std::{
		net::{SocketAddr, ToSocketAddrs},
		os::unix::io::AsRawFd,
		time::{Duration, Instant},
	},
};

#[derive(Debug)]
struct Args {
	listen: bool,
	addr: SocketAddr, // TODO multiple variants
	send_delay: Duration,
	retransmit_delay: Duration,
	send_buffer_size: usize,
	recv_buffer_size: usize,
}

const DEFAULT_BUFFER_SIZE: usize = 8 * 1024 * 1024;

impl Args {
	fn read() -> Result<Args, Box<dyn std::error::Error>> {
		let mut args = pico_args::Arguments::from_env();
		let listen = args.contains("-l");
		let send_buffer_size = args
			.opt_value_from_str("--send-size")?
			.unwrap_or(DEFAULT_BUFFER_SIZE);
		let recv_buffer_size = args
			.opt_value_from_str("--recv-size")?
			.unwrap_or(DEFAULT_BUFFER_SIZE);
		let send_delay =
			Duration::from_secs_f32(args.opt_value_from_str("--send-delay")?.unwrap_or(0.001));
		let retransmit_delay = Duration::from_secs_f32(
			args
				.opt_value_from_str("--retransmit-delay")?
				.unwrap_or(1.0),
		);
		let addr = match args.free()?.get(0) {
			Some(arg) => arg,
			None => {
				return Err(Box::new(simple_error::SimpleError::new(
					"Address was not specified",
				)))
			}
		}
		.to_socket_addrs()?
		.next()
		.unwrap();

		Ok(Args {
			listen,
			addr,
			send_buffer_size,
			recv_buffer_size,
			send_delay,
			retransmit_delay,
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
	let mut connected = false;
	let mut socket = if args.listen {
		UdpSocket::bind(args.addr)?
	} else {
		let socket = UdpSocket::bind("0.0.0.0:0".parse()?)?; // TODO ipv6
		socket.connect(args.addr)?;
		connected = true;
		socket
	};

	const STDIN: Token = Token(0);
	//const STDOUT: Token = Token(1);
	const SOCKET: Token = Token(2);

	let stdin = std::io::stdin();
	let stdin_fd = stdin.as_raw_fd();
	let mut stdin_fd = SourceFd(&stdin_fd);
	poll
		.registry()
		.register(&mut stdin_fd, STDIN, Interest::READABLE)?;

	let mut stdout = std::io::stdout();
	let stdout_fd = stdout.as_raw_fd();
	let mut stdout_fd = SourceFd(&stdout_fd);
	// poll
	// 	.registry()
	// 	.register(&mut stdout_fd, STDOUT, Interest::WRITABLE)?;

	poll
		.registry()
		.register(&mut socket, SOCKET, Interest::WRITABLE | Interest::READABLE)?;

	let mut source = ReadPipe::new(stdin);
	let mut conn = Connection::new(ConnectionParams {
		send_delay: args.send_delay,
		retransmit_delay: args.retransmit_delay,
		send_buffer_size: args.send_buffer_size,
		recv_buffer_size: args.recv_buffer_size,
	});

	let mut events = Events::with_capacity(16);
	let mut sleep_for = None;
	'outer: loop {
		// FIXME this can't sleep for less than 1ms because epoll_wait :(
		trace!("will sleep for {:?}", sleep_for);
		match poll.poll(&mut events, sleep_for) {
			Ok(_) => {}
			Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
				trace!("interrupted");
				continue;
			}
			Err(e) => {
				return Err(Box::new(e));
			}
		}

		let now = Instant::now();

		for event in events.iter() {
			trace!("{:?}", event);
			// if event.is_read_closed() {
			// 	break 'outer;
			// }

			match event.token() {
				SOCKET => {
					if event.is_readable() {
						let mut recv_error = None;
						'packet: loop {
							let mut buf = [0u8; 1500]; // FIXME MTU detection
							let (size, src) = match socket.recv_from(&mut buf) {
								Ok((size, _)) if size == 0 => break 'packet,
								Ok((size, src)) => (size, src),
								Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break 'packet,
								Err(e) => {
									error!("Error receiving UDP packet: {:?}", e);
									recv_error = Some(e);
									break 'packet;
								}
							};

							if !connected {
								socket.connect(src)?;
								connected = true;
							}
							// TODO compare with previous source?

							// FIXME handle errors
							conn.receive_packet(now, &buf[..size])?;
						} // pull all packets

						if conn.data_to_read() > 0 {
							std::io::copy(&mut conn, &mut stdout)?;
							// TODO can stdout block?
						}

						if recv_error.is_some() {
							return Err(Box::new(recv_error.unwrap()));
						}
					}
				}

				STDIN => {
					if event.is_readable() {
						let read = conn.write_from(&mut source)?;
						trace!("read {} bytes from stdin", read);
					}
				}

				// STDOUT => {
				// 	if event.is_writable() {
				// 		std::io::copy(&mut conn, &mut stdout)?;
				// 	}
				// }
				_ => {}
			}
		} // for event

		if connected {
			trace!("now: {:?}, next: {:?}", now, conn.next_packet_time());
			while now >= conn.next_packet_time() {
				let mut buf = [0u8; 1500]; // FIXME MTU detection
				let size = conn.generate_packet(now, &mut buf)?;
				socket.send(&buf[0..size]).unwrap(); // FIXME handle errors
				trace!("send {} bytes", size);
			}
		}

		let next_packet_time = conn.next_packet_time();
		sleep_for = if next_packet_time > now {
			Some(next_packet_time - now)
		} else {
			None
		};
	} // poll loop

	Ok(())
}

fn main() {
	stderrlog::new()
		.module(module_path!())
		.verbosity(6)
		.init()
		.unwrap();

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
		Ok(_) => {}
		Err(e) => {
			error!("Runtime error: {}", e);
		}
	}
}
