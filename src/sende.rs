use {
	crate::sequence::Sequence,
	byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt},
	circbuf::CircBuf,
	//core::num::Wrapping,
	std::{
		collections::VecDeque,
		//error::Error,
		//fmt,
		io::{Cursor, IoSlice, IoSliceMut, Read, Seek, SeekFrom, Write},
		//ops::{Bound, RangeBounds},
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

#[derive(Error, Debug, Clone)]
pub enum FragmentError {
	#[error("Offset too large, must be less than {head:?}")]
	InvalidOffset { head: u32 },
}

#[derive(Debug, PartialEq, Eq)]
struct Fragment {
	offset: u32, // FIXME u64
	size: u8,    // one less: 0-255 -> 1-256
	sent: bool,
}

struct FragmentIteratedMut<'a> {
	fragment: &'a mut Fragment,
	seq: u32,
}

struct FragmentIterator<'a> {
	fragments: std::iter::Rev<std::collections::vec_deque::IterMut<'a, Fragment>>,
	seq: u32,
}

impl<'a> Iterator for FragmentIterator<'a> {
	type Item = FragmentIteratedMut<'a>;

	fn next(&mut self) -> Option<FragmentIteratedMut<'a>> {
		for frag in &mut self.fragments {
			if !frag.sent {
				return Some(FragmentIteratedMut::<'a> {
					fragment: frag,
					seq: self.seq,
				});
			}
			self.seq = self.seq.wrapping_add(1);
		}

		None
	}
}

struct Fragments {
	fragments: VecDeque<Fragment>,
	seq: Sequence, // next fragment sequence number
	               // TODO deque of indexes to fragments to send instead of 'sent' field
}

impl Fragments {
	fn new() -> Fragments {
		Fragments {
			fragments: VecDeque::new(),
			seq: Sequence::new(),
		}
	}

	fn write_chunk(&mut self, size: usize) -> usize {
		let mut size = size;
		let mut count = 0;
		let mut offset = match self.fragments.back() {
			Some(frag) => frag.offset.wrapping_add(frag.size as u32).wrapping_add(1),
			None => 0,
		};
		while size > 0 {
			let fragment_size = std::cmp::min(256, size);
			self.seq.next();
			self.fragments.push_front(Fragment {
				offset,
				size: (fragment_size - 1) as u8,
				sent: false,
			});
			offset = offset.wrapping_add(fragment_size as u32);
			size -= fragment_size;
			count += 1;
		}

		count
	}

	fn iter_unsent_mut(&mut self) -> FragmentIterator {
		let seq: u32 = self.seq.into();
		let seq = seq.wrapping_sub(self.fragments.len() as u32);
		FragmentIterator {
			fragments: self.fragments.iter_mut().rev(),
			seq,
		}
	}

	fn confirm(&mut self, seq: u32) -> Result<usize, FragmentError> {
		// assumption: seq can't be too old to wrap
		let age = match self.seq.age(seq) {
			None => {
				return Err(FragmentError::InvalidOffset {
					head: self.seq.into(),
				})
			}
			Some(age) => age - 1,
		} as usize;

		if age >= self.fragments.len() {
			return Ok(0);
		}

		let to_remove = self.fragments.len() - age;

		let mut size = 0;
		for fragment in self.fragments.iter().rev().take(to_remove) {
			// TODO: consider making Fragment::sent triple state (NotSentEver, Sent, PlsResend)
			// and check whether this confirm is valid for all relevant fragments
			size += fragment.size as usize + 1;
		}

		self.fragments.truncate(age);
		Ok(size)
	}

	// mask lowest bit == start_seq, (mask>>1)&1 == start_seq + 1
	fn mark_for_send(&mut self, start_seq: u32, mask: u64) -> Result<u64, FragmentError> {
		let mut mask = mask;
		let mut sequence = start_seq;
		let mut ret_mask: u64 = 0;
		let mut bit: u64 = 1;
		while mask != 0 {
			println!("{} {:?}", sequence, self.seq);
			let index = match self.seq.age(sequence) {
				None => {
					return Err(FragmentError::InvalidOffset {
						head: self.seq.into(),
					})
				}
				Some(age) => age - 1,
			};

			if mask & 1 == 1 {
				let mut fragment = match self.fragments.get_mut(index as usize) {
					None => continue,
					Some(fragment) => fragment,
				};
				if fragment.sent {
					ret_mask |= bit;
					fragment.sent = false;
				}
			}

			sequence += 1;
			bit <<= 1;
			mask >>= 1;
		}

		Ok(ret_mask)
	}
}

#[derive(Error, Debug, Clone)]
pub enum SenderError {
	#[error("Buffer is too small")]
	BufferTooSmall,
}

pub struct Sender {
	buffer: CircBuf,
	fragments: Fragments,
	consumed_offset: u32, // FIXME u64
	packet_seq: Sequence,
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

impl Sender {
	pub fn new(buffer_capacity: usize) -> Sender {
		Sender {
			buffer: CircBuf::with_capacity(buffer_capacity).unwrap(),
			fragments: Fragments::new(),
			consumed_offset: 0,
			packet_seq: Sequence::new(),
		}
	}

	pub fn write_from(&mut self, source: &mut dyn CircRead) -> std::io::Result<usize> {
		let read = source.read(&mut self.buffer)?;
		if read > 0 {
			self.fragments.write_chunk(read);
		}
		Ok(read)
	}

	pub fn generate(&mut self, buf: &mut [u8]) -> Result<usize, SenderError> {
		// chunks[N]
		// chunk[0..N-2]: u8 size-1, u8 bytes[size]
		// chunk[N-1]: last chunk is metadata:
		//	- u32 packet_sequence
		//	- fragment_ids u32[N-1]

		if buf.len() < 5 {
			return Err(SenderError::BufferTooSmall);
		}

		let mut fragment_ids = [0u32; 16];
		let mut fragments = 0;
		let mut left = buf.len() - 5;
		let mut wr = Cursor::new(buf);
		for it in self.fragments.iter_unsent_mut() {
			if left < 5 || fragments == fragment_ids.len() || fragments == 63 {
				break;
			}

			// FIXME: perf. will almost always traverse ALL fragments. need an unsent/order by size index
			if left - 5 < (it.fragment.size + 1) as usize {
				continue;
			}

			wr.write_u8(it.fragment.size).unwrap();
			// FIXME handle wrapping: both offsets are wrapped u32 in a bigger stream
			assert!(self.consumed_offset <= it.fragment.offset);
			let offset = it.fragment.offset - self.consumed_offset;
			let cursor_offset = wr.position() as usize;
			let mut dest =
				&mut wr.get_mut()[cursor_offset..cursor_offset + it.fragment.size as usize + 1];
			write_from_offset(&mut self.buffer, offset as usize, &mut dest).unwrap();
			wr.set_position((cursor_offset + it.fragment.size as usize + 1) as u64);
			it.fragment.sent = true;
			fragment_ids[fragments] = it.seq;
			fragments += 1;
			left -= 5 + it.fragment.size as usize + 1;
		}

		// FIXME generate header|tail
		let tail_size = (fragments + 1) * 4;
		assert!(tail_size <= 256);
		wr.write_u8((tail_size - 1) as u8).unwrap();
		wr.write_u32::<LittleEndian>(self.packet_seq.next())
			.unwrap();
		for i in 0..fragments {
			wr.write_u32::<LittleEndian>(fragment_ids[i]).unwrap();
		}
		Ok(wr.position() as usize)
	}

	pub fn confirm_read(&mut self, fragment_seq: u32) -> Result<usize, FragmentError> {
		let advance = self.fragments.confirm(fragment_seq)?;
		self.buffer.advance_read(advance);
		Ok(advance)
	}

	fn mark_for_send(&mut self, start_seq: u32, mask: u64) -> Result<u64, FragmentError> {
		self.fragments.mark_for_send(start_seq, mask)
	}

	// 		// 3. offset is correct < bytes sent
	// 		if offset > self.ack_bytes + self.buffer.len() as u64 {
	// 			return Err(std::io::Error::new(
	// 				std::io::ErrorKind::InvalidInput,
	// 				format!(
	// 					"Confirmed read offset {} exceeded theoretical limit of {}",
	// 					offset,
	// 					self.ack_bytes + self.buffer.len() as u64
	// 				),
	// 			));
	// 		}
	//
	// 		if offset <= self.ack_bytes {
	// 			return Ok(0);
	// 		}
	//
	// 		let read = (offset - self.ack_bytes) as usize;
	//
	// 		// 2. erase any fragments before offset
	// 		self.fragments.remove(self.ack_bytes..offset);
	//
	// 		// 1. move ack_bytes
	// 		self.ack_bytes = offset;
	// 		self.buffer.advance_read(read);
	// 		Ok(read)
	// 	}
	//
	// 	pub fn resend(&mut self, offset: u64, size: usize) -> std::io::Result<bool> {
	// 		// 1. check validity:
	// 		//	  a. offset < self.ack_bytes ... ->
	// 		let (offset, size) = if offset < self.ack_bytes {
	// 			let head = self.ack_bytes - offset;
	// 			if head < size as u64 {
	// 				return Ok(false);
	// 			}
	// 			// FIXME overflow analysis
	// 			(self.ack_bytes, size - head as usize)
	// 		} else {
	// 			(offset, size)
	// 		};
	//
	// 		//	  b. offset + size > bytes to transmit
	// 		if offset + size as u64 > self.ack_bytes + self.buffer.len() as u64 {
	// 			return Err(std::io::Error::new(
	// 				std::io::ErrorKind::InvalidInput,
	// 				format!(
	// 					"Resend request ({}, {}) exceeds data we might have sent ({}, {})",
	// 					offset,
	// 					size,
	// 					self.ack_bytes,
	// 					self.buffer.len()
	// 				),
	// 			));
	// 		}
	//
	// 		// 2. insert
	// 		Ok(self.fragments.insert(offset..offset + size as u64))
	// 	}
}

#[cfg(test)]
mod fragment_tests {
	use super::*;

	#[test]
	fn basic_insert_send() {
		let mut fragments = Fragments::new();
		assert_eq!(fragments.write_chunk(337), 2);
		let mut iter = fragments.iter_unsent_mut();
		// TODO seq
		assert_eq!(
			*iter.next().unwrap().fragment,
			Fragment {
				offset: 0,
				size: 255,
				sent: false
			}
		);
		assert_eq!(
			*iter.next().unwrap().fragment,
			Fragment {
				offset: 256,
				size: 80,
				sent: false
			}
		);

		let mut iter = fragments.iter_unsent_mut();
		let mut frag = iter.next().unwrap().fragment;
		assert_eq!(
			*frag,
			Fragment {
				offset: 0,
				size: 255,
				sent: false
			}
		);
		frag.sent = true;
		assert_eq!(
			*iter.next().unwrap().fragment,
			Fragment {
				offset: 256,
				size: 80,
				sent: false
			}
		);
		let mut iter = fragments.iter_unsent_mut();
		assert_eq!(
			*iter.next().unwrap().fragment,
			Fragment {
				offset: 256,
				size: 80,
				sent: false
			}
		);

		assert_eq!(fragments.mark_for_send(0, 1).unwrap(), 1);
		let mut iter = fragments.iter_unsent_mut();
		let frag = iter.next().unwrap().fragment;
		assert_eq!(
			*frag,
			Fragment {
				offset: 0,
				size: 255,
				sent: false
			}
		);
	}

	#[test]
	fn basic_confirm_one() {
		let mut fragments = Fragments::new();
		assert_eq!(fragments.write_chunk(250), 1);
		assert_eq!(fragments.confirm(0).unwrap(), 250);
		assert!(fragments.iter_unsent_mut().next().is_none());
	}

	#[test]
	fn basic_confirm_few() {
		let mut fragments = Fragments::new();
		assert_eq!(fragments.write_chunk(1337), 6);
		assert_eq!(fragments.confirm(2).unwrap(), 256 * 3);
		let mut iter = fragments.iter_unsent_mut();
		assert_eq!(
			*iter.next().unwrap().fragment,
			Fragment {
				offset: 3 * 256,
				size: 255,
				sent: false
			}
		);
		assert_eq!(
			*iter.next().unwrap().fragment,
			Fragment {
				offset: 4 * 256,
				size: 255,
				sent: false
			}
		);
		assert_eq!(
			*iter.next().unwrap().fragment,
			Fragment {
				offset: 5 * 256,
				size: (1337 - 5 * 256) as u8 - 1,
				sent: false
			}
		);
		assert!(iter.next().is_none());
	}

	// FIXME add tests for errors
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
		data: &[u8],
		sent_so_far: &mut usize,
		seq: u32,
		frag_id: u32,
	) {
		assert!(data.len() <= 256);
		let header_size = 1 + 4 + 4 + 1;
		let mut buffer = vec![0u8; data.len() + header_size];
		let packet_size = packetizer.generate(&mut buffer).unwrap();
		assert_eq!(packet_size, header_size + data.len());

		println!("{:?}", &buffer);

		let mut r = Cursor::new(&buffer);
		let chunk_size = r.read_u8().unwrap() as usize + 1;
		assert_eq!(chunk_size, data.len());
		assert_eq!(
			buffer[r.position() as usize..r.position() as usize + chunk_size],
			data[..]
		);

		r.seek(SeekFrom::Current(chunk_size as i64)).unwrap();
		let tail_size = r.read_u8().unwrap();
		assert_eq!(tail_size, 8 - 1);
		let sequence = r.read_u32::<LittleEndian>().unwrap();
		assert_eq!(sequence, seq);
		let fragment_id = r.read_u32::<LittleEndian>().unwrap();
		assert_eq!(fragment_id, frag_id);

		*sent_so_far += data.len();
	}

	// TODO tests:
	// 1. ring buffer full (?! should we grow instead?)

	#[test]
	fn test_basic_packet_formation() {
		let mut sent_so_far = 0 as usize;
		let mut packetizer = Sender::new(32);

		let data = &b"keque...."[..];
		write_data(&mut packetizer, &data[..]);
		check_single_fragment_data(&mut packetizer, &data, &mut sent_so_far, 0, 0);

		let data = &b"qeqkekek"[..];
		write_data(&mut packetizer, &data[..]);
		check_single_fragment_data(&mut packetizer, &data, &mut sent_so_far, 1, 1);
	}

	#[test]
	fn test_basic_packet_formation_with_consume() {
		let mut sent_so_far = 0 as usize;
		let mut packetizer = Sender::new(16);

		let data = &b"keque...."[..];
		write_data(&mut packetizer, &data[..]);
		check_single_fragment_data(&mut packetizer, &data, &mut sent_so_far, 0, 0);

		assert_eq!(packetizer.confirm_read(0).unwrap(), data.len());

		let data = &b"qeqkekek"[..];
		write_data(&mut packetizer, &data[..]);
		check_single_fragment_data(&mut packetizer, &data, &mut sent_so_far, 1, 1);
	}

	#[test]
	fn test_basic_packet_resend() {
		let mut sent_so_far = 0 as usize;
		let mut packetizer = Sender::new(16);

		let data = &b"keque...."[..];
		write_data(&mut packetizer, &data[..]);
		check_single_fragment_data(&mut packetizer, &data, &mut sent_so_far, 0, 0);

		packetizer.mark_for_send(0, 1).unwrap();
		check_single_fragment_data(&mut packetizer, &data, &mut sent_so_far, 1, 0);

		assert_eq!(packetizer.confirm_read(0).unwrap(), data.len());

		sent_so_far = data.len();
		let data = &b"qeqkekek"[..];
		write_data(&mut packetizer, &data[..]);
		check_single_fragment_data(&mut packetizer, &data, &mut sent_so_far, 2, 1);
	}
}
