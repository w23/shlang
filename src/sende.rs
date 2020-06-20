use {
	crate::sequence::Sequence,
	byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt},
	circbuf::CircBuf,
	ranges::{GenericRange, Ranges},
	//core::num::Wrapping,
	std::{
		collections::VecDeque,
		//error::Error,
		//fmt,
		io::{Cursor, IoSlice, IoSliceMut, Read, Seek, SeekFrom, Write},
		ops::{Bound, RangeBounds},
		//vec::Vec,
	},
	thiserror::Error,
};

pub trait CircRead {
	fn read(&mut self, buffer: &mut CircBuf) -> std::io::Result<usize>;
}

pub struct ReadPipe<T: Read> {
	pipe: T,
	readable: bool,
}

impl<T: Read> ReadPipe<T> {
	pub fn new(pipe: T) -> ReadPipe<T> {
		ReadPipe {
			pipe,
			readable: false,
		}
	}
}

impl<T: Read> CircRead for ReadPipe<T> {
	fn read(&mut self, buffer: &mut CircBuf) -> std::io::Result<usize> {
		if buffer.cap() == 0 {
			return Ok(0);
		}
		let [buf1, buf2] = buffer.get_avail();
		let mut bufs = [IoSliceMut::new(buf1), IoSliceMut::new(buf2)];
		match self.pipe.read_vectored(&mut bufs) {
			Ok(0) => {
				self.readable = false;
				Ok(0)
			}
			Ok(read) => {
				buffer.advance_write(read);
				Ok(read)
			}
			Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
				self.readable = false;
				Ok(0)
			}
			Err(e) => Err(e),
		}
	}
}

fn write_from_offset<T: Write>(
	buffer: &mut CircBuf,
	offset: usize,
	dest: &mut T,
) -> std::io::Result<usize> {
	if buffer.len() == 0 {
		return Ok(0);
	}
	let bufs = buffer.get_bytes();
	let len0 = bufs[0].len();
	let len1 = bufs[1].len();
	let off1 = std::cmp::min(if offset < len0 { 0 } else { offset - len0 }, len1);
	let bufs = [
		IoSlice::new(&bufs[0][std::cmp::min(offset, len0)..len0]),
		IoSlice::new(&bufs[1][off1..len1]),
	];
	dest.write_vectored(&bufs)
}

#[derive(Clone, Debug)]
struct Segment {
	offset: u64, // offset from start of the data transfer
	size: usize,
}

impl From<&GenericRange<u64>> for Segment {
	fn from(range: &GenericRange<u64>) -> Segment {
		match (range.start_bound(), range.end_bound()) {
			// FIXME handle: (a) u64 wraparound; (b) size being larger than usize (e.g. 32 bit)
			(Bound::Included(s), Bound::Included(e)) => Segment {
				offset: *s,
				size: (*e - *s + 1) as usize,
			},
			(Bound::Included(s), Bound::Excluded(e)) => Segment {
				offset: *s,
				size: (*e - *s) as usize,
			},
			(Bound::Excluded(s), Bound::Excluded(e)) => Segment {
				offset: *s + 1,
				size: (*e - *s - 1) as usize,
			},
			(Bound::Excluded(s), Bound::Included(e)) => Segment {
				offset: *s + 1,
				size: (*e - *s) as usize,
			},
			_ => panic!("Unbounded range to fragment does not make any sense"),
		}
	}
}

#[derive(Error, Debug, Clone)]
pub enum SenderError {
	#[error("Offset too large, must be less than {head:?}")]
	InvalidOffset { head: u64 },
	#[error("Buffer is too small")]
	BufferTooSmall,
}

pub struct Sender {
	buffer: CircBuf,
	segments: Ranges<u64>,
	buffer_start_offset: u64,
	packet_seq: Sequence,
}

impl Sender {
	pub fn new(buffer_capacity: usize) -> Sender {
		Sender {
			buffer: CircBuf::with_capacity(buffer_capacity).unwrap(),
			segments: Ranges::new(),
			buffer_start_offset: 0,
			packet_seq: Sequence::new(),
		}
	}

	pub fn write_from(&mut self, source: &mut dyn CircRead) -> std::io::Result<usize> {
		let begin = self.buffer_start_offset + self.buffer.len() as u64;
		let read = source.read(&mut self.buffer)?;
		if read > 0 {
			self.segments.insert(begin..begin + read as u64);
		}
		Ok(read)
	}

	pub fn generate(&mut self, buf: &mut [u8]) -> Result<usize, SenderError> {
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
		if buf.len() < 6 {
			return Err(SenderError::BufferTooSmall);
		}

		let mut left = buf.len() - 4;
		let mut wr = Cursor::new(buf);
		wr.write_u32::<LittleEndian>(self.packet_seq.next())
			.unwrap();

		// Iterate through segments to write and build payload
		for it in self.segments.as_slice() {
			if left < 1 + 4 + 8 {
				break;
			}

			let segment: Segment = it.into();
			println!("segment {} {}", segment.offset, segment.size);

			// header
			left -= 2;

			let size = std::cmp::min(segment.size + 8, left);
			assert!(size < 2048);

			let header: u16 = (0u16 << 11) | size as u16;
			wr.write_u16::<LittleEndian>(header).unwrap();

			assert!(self.buffer_start_offset <= segment.offset);

			// offset
			wr.write_u64::<LittleEndian>(segment.offset).unwrap();

			// payload
			let buffer_offset = segment.offset - self.buffer_start_offset;
			let cursor_offset = wr.position() as usize;
			let mut dest = &mut wr.get_mut()[cursor_offset..cursor_offset + (size - 8) as usize];
			write_from_offset(&mut self.buffer, buffer_offset as usize, &mut dest).unwrap();
			wr.seek(SeekFrom::Current((size - 8) as i64)).unwrap();
			left -= size as usize;
		}

		let written = wr.position() as usize;
		let buf = wr.get_mut();

		// Re-read packet and remove written segments from list of segments to write
		println!("{} {}", written, buf.len());
		let mut rd = Cursor::new(&buf[4..written]);
		loop {
			println!("{}", rd.position());
			let header = match rd.read_u16::<LittleEndian>() {
				Ok(value) => value,
				Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
				_ => panic!("Cannot read header of just written packet"),
			};

			println!("{:x}", header);

			let size = (header & 2047) as i64;
			assert!(size > 8);

			let offset = rd.read_u64::<LittleEndian>().unwrap();
			let payload_size = size - 8;

			self.segments.remove(offset..offset + payload_size as u64);
			rd.seek(SeekFrom::Current(payload_size)).unwrap();
		}

		Ok(written)
	}

	pub fn confirm_read(&mut self, offset: u64) -> Result<usize, SenderError> {
		unimplemented!();

		// let advance = self.fragments.confirm(fragment_seq)?;
		// self.buffer.advance_read(advance);
		// Ok(advance)
	}

	pub fn resend(&mut self, offset: u64, size: u16) -> Result<(), SenderError> {
		unimplemented!();
	}
}

#[cfg(test)]
mod packet_tests {
	use super::*;

	fn write_data(packetizer: &mut Sender, data: &[u8]) {
		let mut source = ReadPipe::new(data.as_ref());
		assert_eq!(packetizer.write_from(&mut source).unwrap(), data.len());
	}

	fn check_single_fragment_data(
		packetizer: &mut Sender,
		expect_data: &[u8],
		expect_packet_seq: u32,
		expect_offset: u64,
		sent_so_far: &mut usize,
	) {
		let header_size = 4 + 2 + 8;
		let mut buffer = vec![0u8; expect_data.len() + header_size];
		let packet_size = packetizer.generate(&mut buffer).unwrap();
		assert_eq!(packet_size, header_size + expect_data.len());

		println!("{:?}", &buffer);

		let mut r = Cursor::new(&buffer);
		let packet_seq = r.read_u32::<LittleEndian>().unwrap();
		assert_eq!(expect_packet_seq, packet_seq);

		let chunk_header = r.read_u16::<LittleEndian>().unwrap();
		let chunk_type = chunk_header >> 11;
		assert_eq!(0, chunk_type);

		let chunk_size = (chunk_header & 2047) as usize;
		assert_eq!(chunk_size, expect_data.len() + 8);

		let offset = r.read_u64::<LittleEndian>().unwrap();
		assert_eq!(expect_offset, offset);

		assert_eq!(
			expect_data[..],
			buffer[r.position() as usize..r.position() as usize + chunk_size - 8]
		);

		*sent_so_far += expect_data.len();
	}

	// TODO tests:
	// 1. ring buffer full (?! should we grow instead?)
	// 2. failures

	#[test]
	fn test_basic_packet_formation() {
		let mut sent_so_far = 0 as usize;
		let mut packetizer = Sender::new(32);

		let data = &b"keque...."[..];
		write_data(&mut packetizer, &data[..]);
		check_single_fragment_data(&mut packetizer, &data, 0, 0, &mut sent_so_far);

		let data = &b"qeqkekek"[..];
		write_data(&mut packetizer, &data[..]);
		check_single_fragment_data(
			&mut packetizer,
			&data,
			1,
			sent_so_far as u64,
			&mut sent_so_far,
		);
	}

	#[test]
	fn test_segmented_packets() {
		let mut sent_so_far = 0 as usize;
		let mut packetizer = Sender::new(64);

		let data = &b"IAmALongStringLOOOOOOOOOOOOOOOOOOOOL"[..];
		write_data(&mut packetizer, &data[..]);
		check_single_fragment_data(&mut packetizer, &data[0..17], 0, 0, &mut sent_so_far);
		check_single_fragment_data(&mut packetizer, &data[17..23], 1, 17, &mut sent_so_far);
		check_single_fragment_data(&mut packetizer, &data[23..], 2, 23, &mut sent_so_far);
	}

	// #[test]
	// fn test_basic_packet_formation_with_consume() {
	// 	let mut sent_so_far = 0 as usize;
	// 	let mut packetizer = Sender::new(16);
	//
	// 	let data = &b"keque...."[..];
	// 	write_data(&mut packetizer, &data[..]);
	// 	check_single_fragment_data(&mut packetizer, &data, &mut sent_so_far, 0, 0);
	//
	// 	assert_eq!(packetizer.confirm_read(0).unwrap(), data.len());
	//
	// 	let data = &b"qeqkekek"[..];
	// 	write_data(&mut packetizer, &data[..]);
	// 	check_single_fragment_data(&mut packetizer, &data, &mut sent_so_far, 1, 1);
	// }
	//
	// #[test]
	// fn test_basic_packet_resend() {
	// 	let mut sent_so_far = 0 as usize;
	// 	let mut packetizer = Sender::new(16);
	//
	// 	let data = &b"keque...."[..];
	// 	write_data(&mut packetizer, &data[..]);
	// 	check_single_fragment_data(&mut packetizer, &data, &mut sent_so_far, 0, 0);
	//
	// 	packetizer.mark_for_send(0, 1).unwrap();
	// 	check_single_fragment_data(&mut packetizer, &data, &mut sent_so_far, 1, 0);
	//
	// 	assert_eq!(packetizer.confirm_read(0).unwrap(), data.len());
	//
	// 	sent_so_far = data.len();
	// 	let data = &b"qeqkekek"[..];
	// 	write_data(&mut packetizer, &data[..]);
	// 	check_single_fragment_data(&mut packetizer, &data, &mut sent_so_far, 2, 1);
	// }
}
