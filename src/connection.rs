use {
	crate::{receive::*, sende::*, sequence::Sequence},
	byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt},
	log::error,
	std::{
		cmp::min,
		error::Error,
		io::{Cursor, ErrorKind, Seek, SeekFrom},
		time::{Duration, Instant},
	},
	//thiserror::Error,
};

pub struct Connection {
	mtu: usize,
	send_delay: Duration,
	retransmit_delay: Duration,

	packet_seq: Sequence,

	sender: Sender,
	next_send_time: Instant,

	receiver: Receiver,
}

pub struct ConnectionParams {
	mtu: usize,
	send_delay: Duration,
	retransmit_delay: Duration,
	send_buffer_size: usize,
	recv_buffer_size: usize,
}

impl Connection {
	pub fn new(params: ConnectionParams) -> Connection {
		let now = Instant::now();
		Self {
			mtu: params.mtu,
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
		// u32 packet sequence number
		// chunks[]:
		//  - u16 head:
		//   	- high 4 bits: type/flags
		//   	- low 11 bits: chunk size
		//  - u8 data[chunk size]
		//   	- type == 0
		//      - u64 offset
		//      - u8 payload[chunk size - 8]

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

		Ok(header_size + feedback_size + payload_size)
	}

	pub fn write_from(&mut self, source: &mut dyn CircRead) -> std::io::Result<usize> {
		self.sender.write_from(source)
	}
}

impl std::io::Read for Connection {
	fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		self.receiver.read(buf)
	}
}

// #[cfg(test)]
// mod tests {
// 	use super::*;
//
// 	use rand::rngs::SmallRng;
// 	use rand::seq::SliceRandom;
// 	use rand::{Rng, RngCore, SeedableRng};
//
// 	#[test]
// 	fn send_receive_ideal_ordered() {
// 		let mut rng = SmallRng::from_seed([0u8; 16]);
// 		let mut data = vec![0u8; 1 * 1024 * 1024];
// 		rng.fill_bytes(&mut data);
// 		let data = &data;
//
// 		let mut sender = Sender::new(8 * 1024 * 1024);
// 		let mut receiver = Receiver::new(); //1024 * 1024 / 256 + 1);
//
// 		let mut read_pipe = ReadPipe::new(&data[..]);
// 		assert_eq!(sender.write_from(&mut read_pipe).unwrap(), data.len());
//
// 		let mut offset = 0;
// 		loop {
// 			let mut buf = [0u8; 1500];
// 			let size = sender.generate(&mut buf).unwrap();
//
// 			println!("{}", size);
// 			if size == 12 {
// 				// Means no chunks
// 				break;
// 			}
// 			receiver.receive_packet(&buf[..size]).unwrap();
//
// 			let read = receiver.read(&mut buf).unwrap();
// 			println!("read {}", read);
// 			assert_eq!(&data[offset..offset + read], &buf[..read]);
// 			offset += read;
// 		}
// 		assert_eq!(offset, data.len());
// 	}
//
// 	#[test]
// 	fn send_receive_random_order() {
// 		let mut rng = SmallRng::from_seed([0u8; 16]);
// 		let mut data = vec![0u8; 1 * 1024 * 1024];
// 		rng.fill_bytes(&mut data);
// 		let data = &data;
//
// 		let mut sender = Sender::new(8 * 1024 * 1024);
// 		let mut receiver = Receiver::new(); //1024 * 1024 / 256 + 1);
//
// 		let mut read_pipe = ReadPipe::new(&data[..]);
// 		assert_eq!(sender.write_from(&mut read_pipe).unwrap(), data.len());
//
// 		let mut sent = false;
// 		let mut packets = Vec::<([u8; 1500], usize)>::new();
// 		let mut offset = 0;
// 		loop {
// 			let mut buf = [0u8; 1500];
// 			if !sent {
// 				let size = sender.generate(&mut buf).unwrap();
//
// 				println!("{}", size);
// 				if size == 12 {
// 					// Means no chunks
// 					sent = true;
// 				} else {
// 					packets.push((buf, size));
// 				}
// 			}
//
// 			if sent || packets.len() > 7 {
// 				packets.shuffle(&mut rng);
// 				let (packet, size) = match packets.pop() {
// 					Some(packet) => packet,
// 					None => break,
// 				};
// 				receiver.receive_packet(&packet[..size]).unwrap();
// 				loop {
// 					let read = receiver.read(&mut buf).unwrap();
// 					println!("read {}", read);
// 					if read == 0 {
// 						break;
// 					}
// 					assert_eq!(&data[offset..offset + read], &buf[..read]);
// 					offset += read;
// 				}
// 			}
// 		}
// 		assert_eq!(offset, data.len());
// 	}
//
// 	#[test]
// 	fn send_receive_lost_10pct() {
// 		let mut rng = SmallRng::from_seed([0u8; 16]);
// 		let mut data = vec![0u8; 1 * 1024 * 1024];
// 		rng.fill_bytes(&mut data);
// 		let data = &data;
//
// 		let mut sender = Sender::new(8 * 1024 * 1024);
// 		let mut receiver = Receiver::new(); //1024 * 1024 / 256 + 1);
//
// 		let mut read_pipe = ReadPipe::new(&data[..]);
// 		assert_eq!(sender.write_from(&mut read_pipe).unwrap(), data.len());
//
// 		let mut counter = 0;
// 		let mut offset = 0;
// 		loop {
// 			let mut buf = [0u8; 1500];
// 			let size = sender.generate(&mut buf).unwrap();
// 			if size > 12 {
// 				println!("{}", size);
// 				if rng.gen_range(0, 100) > 9 {
// 					receiver.receive_packet(&buf[..size]).unwrap();
// 				}
// 			}
//
// 			counter += 1;
// 			if counter % 10 == 9 {
// 				let size = receiver.gen_feedback_packet(&mut buf).unwrap();
// 				println!("feedback: {:?}", &buf[..size]);
// 				sender.receive_feedback(&buf[..size]).unwrap();
// 			}
//
// 			loop {
// 				let read = receiver.read(&mut buf).unwrap();
// 				println!("read {}", read);
// 				if read == 0 {
// 					break;
// 				}
// 				assert_eq!(&data[offset..offset + read], &buf[..read]);
// 				offset += read;
// 			}
//
// 			if dbg!(offset) == dbg!(data.len()) {
// 				break;
// 			}
// 		}
// 		assert_eq!(offset, data.len());
// 	}
//
// 	// TODO:
// 	// 3. packet loss + random order
// }
