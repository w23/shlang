use {
	crate::{receive::*, sende::*, sequence::Sequence},
	byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt},
	log::error,
	std::{
		cmp::min,
		error::Error,
		io::{Cursor, ErrorKind, Read, Seek, SeekFrom},
		time::{Duration, Instant},
	},
	//thiserror::Error,
};

pub struct Connection {
	// mtu: usize,
	send_delay: Duration,
	retransmit_delay: Duration,

	packet_seq: Sequence,

	sender: Sender,
	next_send_time: Instant,

	receiver: Receiver,
}

pub struct ConnectionParams {
	// mtu: usize,
	send_delay: Duration,
	retransmit_delay: Duration,
	send_buffer_size: usize,
	recv_buffer_size: usize,
}

impl Connection {
	pub fn new(params: ConnectionParams) -> Connection {
		let now = Instant::now();
		Self {
			// mtu: params.mtu,
			send_delay: params.send_delay,
			retransmit_delay: params.retransmit_delay,

			packet_seq: Sequence::new(),

			sender: Sender::new(params.send_buffer_size),
			next_send_time: now,

			receiver: Receiver::new(params.recv_buffer_size),
		}
	}

	// u32 packet sequence number
	// chunks[]:
	//  - u16 head:
	//   	- high 4 bits: type/flags
	//   	- low 11 bits: chunk size
	//  - u8 data[chunk size]
	//   	- type == 0
	//      - u64 offset
	//      - u8 payload[chunk size - 8]
	//   	- type == 1
	//      - u64 confirm_offset
	//      - [] chunks to resend
	//      	- u64 offset
	//      	- u16 size

	pub fn receive_packet(&mut self, now: Instant, packet: &[u8]) -> Result<(), Box<dyn Error>> {
		let mut rd = Cursor::new(&packet);
		let packet_seq = rd.read_u32::<LittleEndian>()?;

		let stream_front = rd.read_u64::<LittleEndian>()?;
		self.receiver.update_stream_front(stream_front, now);

		loop {
			let chunk_head = if let Ok(v) = rd.read_u16::<LittleEndian>() {
				v
			} else {
				break;
			};
			let chunk_type = (chunk_head >> 11) as usize;
			let chunk_size = (chunk_head & 2047) as usize;

			println!("chunk type={} size={}", chunk_type, chunk_size);

			let offset = rd.position() as usize;
			if offset + chunk_size > packet.len() {
				// TODO might have handled some chunks, how to report?
				// TODO better error reporting vs logging
				println!(
					"Chunk type {} size {} at offset {} ends abruptly: left {} bytes in packet",
					chunk_type,
					chunk_size,
					offset,
					packet.len() - offset,
				);
				return Err(Box::new(SenderError::IoError {
					kind: ErrorKind::UnexpectedEof,
				}));
			}
			let chunk_data = &packet[offset..offset + chunk_size];
			rd.seek(SeekFrom::Current(chunk_size as i64)).unwrap();

			println!(
				"chunk @{} sz={} contents:{:?}",
				offset, chunk_size, chunk_data
			);

			match chunk_type {
				0 => {
					// Regular data segment chunk
					self.receiver.read_chunk_segment(chunk_data, now)?;
				}
				1 => {
					// Feedback chunk
					self.sender.read_chunk_feedback(chunk_data)?;
				}
				_ => {
					// TODO might have handled some chunks, how to report?
					error!("Unknown chunk type {} with size {}", chunk_type, chunk_size);
					return Err(Box::new(SenderError::IoError {
						kind: ErrorKind::InvalidData,
					}));
				}
			}
		}

		Ok(())
	}

	pub fn next_packet_time(&self) -> Instant {
		self.next_send_time
	}

	pub fn generate_packet(&mut self, packet: &mut [u8]) -> Result<usize, Box<dyn Error>> {
		// check that there's enough space to write at least one header
		if packet.len() < 14 {
			return Err(Box::new(SenderError::BufferTooSmall));
		}

		let mut left = packet.len() - 4;
		let mut wr = Cursor::new(&mut packet[..]);
		wr.write_u32::<LittleEndian>(self.packet_seq.next())
			.unwrap();

		left -= 8;
		wr.write_u64::<LittleEndian>(self.sender.stream_front())
			.unwrap();

		let header_size = wr.position() as usize;

		// FIXME:
		// 1. Generate feedback
		let feedback_size = self
			.receiver
			.make_chunk_feedback(&mut packet[header_size..header_size + min(left, 128)])?;

		// 2. Payload
		let payload_size = self
			.sender
			.make_chunk_payload(&mut packet[header_size + feedback_size..])?;

		println!(
			"generated: {}, {}, {}",
			header_size, feedback_size, payload_size
		);

		Ok(header_size + feedback_size + payload_size)
	}

	pub fn write_from(&mut self, source: &mut dyn CircRead) -> std::io::Result<usize> {
		self.sender.write_from(source)
	}

	pub fn send_left(&self) -> usize {
		self.sender.len()
	}
}

impl Read for Connection {
	fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		self.receiver.read(buf)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	use rand::rngs::SmallRng;
	use rand::seq::SliceRandom;
	use rand::{Rng, RngCore, SeedableRng};

	trait NetworkEmulator {
		fn transmit(&mut self, src: &mut Connection, dst: &mut Connection) -> bool;
	}

	struct IdealNetwork {}
	impl NetworkEmulator for IdealNetwork {
		fn transmit(&mut self, src: &mut Connection, dst: &mut Connection) -> bool {
			let now = Instant::now();
			let mut buf = [0u8; 1500];
			let size = src.generate_packet(&mut buf).unwrap();
			println!("New packet size = {}", size);
			dst.receive_packet(now, &buf[..size]).unwrap();

			src.send_left() != 0
		}
	}

	struct BufferedReorderNetwork {
		rng: SmallRng,
		packets: Vec<Box<Vec<u8>>>,
		min_size: usize,
		min_size_reached: bool,
	}

	impl BufferedReorderNetwork {
		fn new(seed: u64, min_size: usize) -> Self {
			BufferedReorderNetwork {
				rng: SmallRng::seed_from_u64(seed),
				packets: Vec::new(),
				min_size,
				min_size_reached: false,
			}
		}
	}

	impl NetworkEmulator for BufferedReorderNetwork {
		fn transmit(&mut self, src: &mut Connection, dst: &mut Connection) -> bool {
			loop {
				let now = Instant::now();
				let mut buf = [0u8; 1500];
				let size = src.generate_packet(&mut buf).unwrap();
				self.packets.push(Box::new(buf[..size].to_vec()));
				println!("New packet size={}", size);

				if self.min_size_reached {
					// FIXME (perf) just choose random slot instead
					self.packets.shuffle(&mut self.rng);
					let packet = match self.packets.pop() {
						Some(packet) => packet,
						None => break,
					};
					dst.receive_packet(now, &packet).unwrap();
					break;
				}
				if self.packets.len() >= self.min_size {
					self.min_size_reached = true;
					break;
				}
			}

			src.send_left() != 0
		}
	}

	struct LosingPacketsNetwork {
		rng: SmallRng,
		percent: u8,
	}

	impl LosingPacketsNetwork {
		fn new(seed: u64, percent: u8) -> Self {
			Self {
				rng: SmallRng::seed_from_u64(seed),
				percent,
			}
		}
	}
	impl NetworkEmulator for LosingPacketsNetwork {
		fn transmit(&mut self, src: &mut Connection, dst: &mut Connection) -> bool {
			let now = Instant::now();
			let mut buf = [0u8; 1500];
			let size = src.generate_packet(&mut buf).unwrap();
			println!("New packet size = {}", size);
			if self.rng.gen_range(0, 100) > self.percent {
				dst.receive_packet(now, &buf[..size]).unwrap();
			}

			src.send_left() != 0
		}
	}

	fn run_single_transfer_test(
		seed: u64,
		size: usize,
		buffer_size: usize,
		src_dst: &mut dyn NetworkEmulator,
		dst_src: &mut dyn NetworkEmulator,
	) {
		let mut rng = SmallRng::seed_from_u64(seed);
		let mut data = vec![0u8; size];
		rng.fill_bytes(&mut data);
		let data = &data;

		let mut sender = Connection::new(ConnectionParams {
			send_delay: Duration::from_millis(1),
			retransmit_delay: Duration::from_secs(1),
			send_buffer_size: buffer_size,
			recv_buffer_size: buffer_size,
		});

		let mut receiver = Connection::new(ConnectionParams {
			send_delay: Duration::from_millis(1),
			retransmit_delay: Duration::from_secs(1),
			send_buffer_size: buffer_size,
			recv_buffer_size: buffer_size,
		});

		let mut read_pipe = ReadPipe::new(&data[..]);

		let mut offset = 0;
		loop {
			sender.write_from(&mut read_pipe).unwrap();

			if !src_dst.transmit(&mut sender, &mut receiver) {
				break;
			}

			dst_src.transmit(&mut receiver, &mut sender);

			let mut buf = [0u8; 1500];
			let read = receiver.read(&mut buf).unwrap();
			println!("read {}", read);
			assert_eq!(&data[offset..offset + read], &buf[..read]);
			offset += read;
		}
		assert_eq!(offset, data.len());
	}

	#[test]
	fn send_receive_ideal_ordered() {
		run_single_transfer_test(
			1,
			1024 * 1024,
			8 * 1024,
			&mut IdealNetwork {},
			&mut IdealNetwork {},
		);
	}

	#[test]
	fn send_receive_reordered() {
		run_single_transfer_test(
			1,
			1024 * 1024,
			32 * 1024,
			&mut BufferedReorderNetwork::new(2, 16),
			&mut IdealNetwork {},
		);
	}

	#[test]
	fn send_receive_loss_10pct() {
		run_single_transfer_test(
			1,
			1024 * 1024,
			32 * 1024,
			&mut LosingPacketsNetwork::new(2, 10),
			&mut IdealNetwork {},
		);
	}

	#[test]
	fn send_receive_loss_90pct() {
		run_single_transfer_test(
			1,
			1024 * 1024,
			32 * 1024,
			&mut LosingPacketsNetwork::new(2, 90),
			&mut IdealNetwork {},
		);
	}

	// TODO:
	// 3. packet loss + random order
	// 4. all of the same + duplex transfer
}
