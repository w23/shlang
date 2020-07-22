mod OchenCircusBuf;
mod connection;
mod receive;
mod sende;
mod sequence;

use {
	connection::{Connection, ConnectionParams},
	libc::{fcntl, F_GETFL, F_SETFL, O_NONBLOCK},
	log::{debug, error, info, trace /* warn */},
	mio::{net::UdpSocket, unix::SourceFd, Events, Interest, Poll, Token},
	sende::ReadPipe,
	std::{
		net::{SocketAddr, ToSocketAddrs},
		os::unix::io::{AsRawFd, RawFd},
		time::{Duration, Instant},
	},
};

#[derive(Debug)]
struct Args {
	verbosity: usize,
	listen: bool,
	addr: SocketAddr, // TODO multiple variants
	mtu: usize,
	send_kbits_per_second: usize,
	// TODO expected RTT, packet loss, max packet delay in flight
	// 			 => compute: buffer sizes, retransmit delay, feedback rate
	retransmit_delay: Duration,
	send_buffer_size: usize,
	recv_buffer_size: usize,
}

const DEFAULT_VERBOSITY: usize = 3;
const DEFAULT_BUFFER_SIZE: usize = 8 * 1024 * 1024;

impl Args {
	fn read() -> Result<Args, Box<dyn std::error::Error>> {
		let mut args = pico_args::Arguments::from_env();
		let listen = args.contains("-l");
		let verbosity = args.opt_value_from_str("-v")?.unwrap_or(DEFAULT_VERBOSITY);
		let send_buffer_size = args
			.opt_value_from_str("--send-size")?
			.unwrap_or(DEFAULT_BUFFER_SIZE);
		let recv_buffer_size = args
			.opt_value_from_str("--recv-size")?
			.unwrap_or(DEFAULT_BUFFER_SIZE);
		let mtu = args.opt_value_from_str("--mtu")?.unwrap_or(1500);
		let send_kbits_per_second = args.opt_value_from_str("--send-rate")?.unwrap_or(1024);
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
			verbosity,
			listen,
			addr,
			send_buffer_size,
			recv_buffer_size,
			mtu,
			send_kbits_per_second,
			retransmit_delay,
		})
	}

	fn print_usage() {
		let name = std::env::args().next().unwrap();
		println!("Usage: {} [options] addr", name);
		println!("\t-l -- listen mode");
	}
}

unsafe fn set_nonblocking(fd: RawFd) {
	let flags = fcntl(fd, F_GETFL, 0);
	if flags < 0 {
		panic!(
			"Couldn't get fd {} flags: {}",
			fd,
			std::io::Error::last_os_error()
		);
	}

	let flags = flags | O_NONBLOCK;
	let result = fcntl(fd, F_SETFL, flags);
	if result < 0 {
		panic!(
			"Couldn't set fd {} O_NOBLOCK flag: {}",
			fd,
			std::io::Error::last_os_error()
		);
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
	unsafe {
		set_nonblocking(stdin_fd);
	}
	let mut stdin_source = SourceFd(&stdin_fd);
	poll
		.registry()
		.register(&mut stdin_source, STDIN, Interest::READABLE)?;

	let mut stdout = std::io::stdout();
	let stdout_fd = stdout.as_raw_fd();
	let mut stdout_fd = SourceFd(&stdout_fd);
	// poll
	// 	.registry()
	// 	.register(&mut stdout_fd, STDOUT, Interest::WRITABLE)?;

	poll
		.registry()
		.register(&mut socket, SOCKET, Interest::WRITABLE | Interest::READABLE)?;

	let send_delay =
		Duration::from_secs_f32((8.0 * args.mtu as f32) / (1024 * args.send_kbits_per_second) as f32);

	info!("{:?}", send_delay);

	let mut source = ReadPipe::new(stdin);
	let mut conn = Connection::new(ConnectionParams {
		send_delay,
		retransmit_delay: args.retransmit_delay,
		send_buffer_size: args.send_buffer_size,
		recv_buffer_size: args.recv_buffer_size,
	});

	let mut buf = vec![0u8; args.mtu];
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

							trace!("packet received {}", size);

							if !connected {
								socket.connect(src)?;
								connected = true;
							}
							// TODO compare with previous source?
							// TODO survive peer changing ip?

							// FIXME handle errors
							conn.receive_packet(now, &buf[..size])?;

							if conn.data_to_read() > 0 {
								let written = std::io::copy(&mut conn, &mut stdout)?;
								debug!("written to stdout: {}", written);
								// TODO can stdout block?
							}
						} // pull all packets

						if recv_error.is_some() {
							return Err(Box::new(recv_error.unwrap()));
						}
					}
				}

				STDIN => {
					source.readable = event.is_readable();
				}

				// STDOUT => {
				// 	if event.is_writable() {
				// 		std::io::copy(&mut conn, &mut stdout)?;
				// 	}
				// }
				_ => {}
			}
		} // for event

		// FIXME proper readable tracker
		if source.readable && conn.buffer_free() > 0 {
			let read = conn.write_from(&mut source)?;
			trace!(
				"read {} bytes from stdin, free: {}",
				read,
				conn.buffer_free()
			);
		}

		if connected {
			// TODO limit number of packets generated in one even loop cycle
			let num_packets = conn.packets_available(now);
			for _ in 0..num_packets {
				let size = conn.generate_packet(&mut buf)?;
				socket.send(&buf[0..size]).unwrap(); // FIXME handle errors, esp WOULDBLOCK
				trace!("send {} bytes", size);
				if !conn.have_data_to_send() {
					break;
				}
			}
			if num_packets > 0 {
				conn.update_sent_time(now);
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
	let args = match Args::read() {
		Ok(args) => args,
		Err(e) => {
			error!("Error parsing args: {}", e);
			Args::print_usage();
			return;
		}
	};

	stderrlog::new()
		.module(module_path!())
		.verbosity(args.verbosity)
		.init()
		.unwrap();

	info!("{:?}", args);

	match run(&args) {
		Ok(_) => {}
		Err(e) => {
			error!("Runtime error: {}", e);
		}
	}
}
