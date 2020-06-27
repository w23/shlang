use {
	crate::sequence::Sequence,
	byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt},
	log::{debug, error, info, trace, warn},
	std::{
		collections::VecDeque,
		io::{Cursor, Error, ErrorKind, Read, Seek, SeekFrom, Write},
		time::Instant,
	},
	thiserror::Error,
};

use crate::OchenCircusBuf::OchenCircusBuf;

#[derive(Error, Debug, Clone, PartialEq)]
enum SegmentationFault {
	#[error("Attempted to insert missing segment not at the head {head_offset:?}")]
	TooOld { head_offset: u64 },
	// #[error("Fragment sequence is too new, expected older than {newest_sequence:?}")]
	// TooNew { newest_sequence: u32 },
	// #[error("Fragment already received")]
	// AlreadyReceived,
	// #[error("Fragment already received and has size that differs {known_size:?}")]
	// Inconsistent { known_size: u8 },
}

#[derive(Debug, Clone, PartialEq)]
struct MissingSegment {
	// global offsets
	begin: u64,
	end: u64,

	// expected to have been received at this time
	timestamp: Instant,
}

struct MissingSegmentIterator<'a> {
	segments: &'a mut Vec<MissingSegment>,
	next: usize,
}

impl<'a> MissingSegmentIterator<'a> {
	fn new(segments: &'a mut Vec<MissingSegment>) -> Self {
		Self { segments, next: 0 }
	}

	fn next(&mut self) -> Option<&MissingSegment> {
		if self.next >= self.segments.len() {
			return None;
		}

		let item = &self.segments[self.next];
		self.next += 1;
		Some(item)
	}

	fn update_timestamp(&mut self, timestamp: Instant) -> Option<Instant> {
		if self.next > self.segments.len() || self.next == 0 {
			return None;
		}

		let item = &mut self.segments[self.next - 1];
		let old_timestamp = item.timestamp;
		item.timestamp = timestamp;
		Some(old_timestamp)
	}

	fn cut_range(&mut self, cut_begin: u64, cut_end: u64, timestamp: Instant) {
		// -> Result<(), SegmentationFault> {
		if self.next > self.segments.len() || self.next == 0 {
			panic!("Cutting an invalid iterator");
		}

		let item = &mut self.segments[self.next - 1];
		if item.begin > cut_begin || item.end < cut_end {
			panic!("Invalid range, should be strictly within");
		}

		if cut_begin == item.begin && cut_end == item.end {
			// Case 1: removing the entire missing segment (not missing anymore!)
			self.segments.remove(self.next - 1);
			self.next -= 1;
		} else if cut_begin == item.begin {
			// Case 2a: removing first part of missing segment
			item.begin = cut_end;
		} else if cut_end == item.end {
			// Case 2b: removing last part of missing segment
			item.end = cut_begin;
			item.timestamp = timestamp;
		} else {
			// Case 3: splitting missing segment in two
			let item_end = item.end;
			let item_timestamp = item.timestamp;
			item.timestamp = timestamp;
			item.end = cut_begin;
			self.segments.insert(
				self.next,
				MissingSegment {
					begin: cut_end,
					end: item_end,
					timestamp: item_timestamp,
				},
			);
			self.next += 1;
		}
	}
}

#[derive(Debug)]
struct MissingSegments {
	segments: Vec<MissingSegment>,
}

impl MissingSegments {
	fn new() -> Self {
		Self {
			segments: Vec::new(),
		}
	}

	// Insert new known missing segment
	// Only supports adding at the head
	fn insert(&mut self, begin: u64, end: u64, timestamp: Instant) -> Result<(), SegmentationFault> {
		match self.segments.last() {
			Some(segment) if segment.end > begin => {
				return Err(SegmentationFault::TooOld {
					head_offset: segment.end,
				})
			}
			_ => {}
		}

		self.segments.push(MissingSegment {
			begin,
			end,
			timestamp,
		});
		Ok(())
	}

	fn missing_iter(&mut self) -> MissingSegmentIterator {
		MissingSegmentIterator::new(&mut self.segments)
	}
}

#[cfg(test)]
mod segment_tests {
	use super::*;

	#[test]
	fn insert_iter() {
		let now = Instant::now();
		let mut segs = MissingSegments::new();
		segs.insert(10, 20, now).unwrap();
		segs.insert(30, 40, now).unwrap();

		let mut iter = segs.missing_iter();
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 10,
				end: 20,
				timestamp: now
			}
		);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 30,
				end: 40,
				timestamp: now
			}
		);
		assert!(iter.next().is_none());

		assert_eq!(
			segs.insert(23, 27, now).err().unwrap(),
			SegmentationFault::TooOld { head_offset: 40 }
		);
	}

	#[test]
	fn cutlery() {
		let now = Instant::now();
		let mut segs = MissingSegments::new();
		segs.insert(10, 20, now).unwrap();
		segs.insert(30, 40, now).unwrap();
		segs.insert(50, 80, now).unwrap();
		segs.insert(90, 100, now).unwrap();

		let mut iter = segs.missing_iter();
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 10,
				end: 20,
				timestamp: now
			}
		);
		let now2 = Instant::now();
		iter.cut_range(10, 15, now2);

		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 30,
				end: 40,
				timestamp: now
			}
		);

		let mut iter = segs.missing_iter();
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 15,
				end: 20,
				timestamp: now
			}
		);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 30,
				end: 40,
				timestamp: now
			}
		);
		let now3 = Instant::now();
		iter.cut_range(37, 40, now3);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 50,
				end: 80,
				timestamp: now
			}
		);

		let mut iter = segs.missing_iter();
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 15,
				end: 20,
				timestamp: now
			}
		);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 30,
				end: 37,
				timestamp: now3
			}
		);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 50,
				end: 80,
				timestamp: now
			}
		);
		let now4 = Instant::now();
		iter.cut_range(60, 70, now4);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 90,
				end: 100,
				timestamp: now
			}
		);

		let mut iter = segs.missing_iter();
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 15,
				end: 20,
				timestamp: now
			}
		);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 30,
				end: 37,
				timestamp: now3
			}
		);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 50,
				end: 60,
				timestamp: now4
			}
		);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 70,
				end: 80,
				timestamp: now
			}
		);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 90,
				end: 100,
				timestamp: now
			}
		);
		assert!(iter.next().is_none());
	}
}

pub struct Receiver {
	packet_seq: Sequence,
	missing: MissingSegments,
	buffer: OchenCircusBuf,
	buffer_start_offset: u64,
}

impl Receiver {
	pub fn new() -> Receiver {
		Receiver {
			packet_seq: Sequence::new(),
			missing: MissingSegments::new(),
			buffer: OchenCircusBuf::with_capacity(1024 * 1024),
			buffer_start_offset: 0,
		}
	}

	pub fn receive_packet(&mut self, data: &[u8]) -> std::io::Result<usize> {
		let now = Instant::now(); // TODO receive as argument?

		// u32 packet sequence number
		// chunks[]:
		//  - u16 head:
		//   	- high 4 bits: type/flags
		//   	- low 11 bits: chunk size
		//  - u8 data[chunk size]
		//   	- type == 0
		//      - u64 offset
		//      - u8 payload[chunk size - 8]

		let mut rd = Cursor::new(&data);
		let packet_seq = rd.read_u32::<LittleEndian>()?;

		'chunks: loop {
			let chunk_head = if let Ok(v) = rd.read_u16::<LittleEndian>() {
				v
			} else {
				break;
			};
			let chunk_type = (chunk_head >> 11) as usize;
			let chunk_size = (chunk_head & 2047) as usize;
			let offset = rd.position() as usize;
			if offset + chunk_size > data.len() {
				// TODO might have handled some chunks, how to report?
				return Err(Error::new(ErrorKind::UnexpectedEof, "Chunk ends abruptly"));
			}
			let chunk_data = &data[offset..offset + chunk_size];

			match chunk_type {
				0 => {
					// Regular data segment chunk
					let mut segment_begin = rd.read_u64::<LittleEndian>().unwrap();
					let mut segment_data = &chunk_data[8..chunk_size - 8];
					rd.seek(SeekFrom::Current(segment_data.len() as i64))
						.unwrap();

					let mut segment_end = segment_begin + segment_data.len() as u64;
					if segment_end <= self.buffer_start_offset {
						// Segment is too old, skip
						continue;
					}

					if segment_begin < self.buffer_start_offset {
						let shift = (self.buffer_start_offset - segment_begin) as usize;
						segment_begin = self.buffer_start_offset;
						segment_data = &segment_data[shift..segment_data.len() - shift];
					}

					let mut missing_iter = self.missing.missing_iter();
					'missing: loop {
						let missing = match missing_iter.next() {
							Some(item) => item,
							_ => break 'missing,
						};

						// missing segment is fully before received segment
						if missing.end <= segment_begin {
							continue 'missing;
						}

						// Missing segment is fully after received segment
						if missing.begin >= segment_end {
							// TODO count duplicate received data
							continue 'chunks;
						}

						// Handle missing & received
						// Find begin intersection
						if missing.begin > segment_begin {
							let shift = (missing.begin - segment_begin) as usize;
							segment_begin = missing.begin;
							segment_data = &segment_data[shift..segment_data.len() - shift];
						};

						let write_end = std::cmp::min(missing.end, segment_end);
						let to_write_size = (segment_end - write_end) as usize;
						// Write received to buffer
						let buf_offset = (segment_begin - self.buffer_start_offset) as usize;
						let written = self
							.buffer
							.write_data_at_read_offset(buf_offset, &segment_data[..to_write_size]);

						if written < to_write_size {
							panic!(format!("Inconsistent missing vs buffer state. Could write only {} bytes of {} in the middle, offset = {}, global offset = {}", written, to_write_size, buf_offset, self.buffer_start_offset));
						}

						// Update missing
						missing_iter.cut_range(segment_begin, write_end, now);
						//.unwrap();

						// Update segment and early exit if empty
						let shift = (write_end - segment_begin) as usize;
						segment_begin = write_end;
						segment_data = &segment_data[shift..segment_data.len() - shift];
						if segment_data.len() == 0 {
							continue 'chunks;
						}
					}

					// handle head
					let buffer_head = self.buffer_start_offset + self.buffer.len() as u64;
					if segment_end > buffer_head {
						if segment_begin > buffer_head {
							self
								.missing
								.insert(buffer_head, segment_begin, now)
								.unwrap();
						} else {
							let shift = (buffer_head - segment_begin) as usize;
							segment_begin = buffer_head;
							segment_data = &segment_data[shift..segment_data.len() - shift];
						}

						if segment_data.len() == 0 {
							continue 'chunks;
						}

						// Write received to buffer
						let buf_offset = (segment_begin - self.buffer_start_offset) as usize;
						let written = self
							.buffer
							.write_data_at_read_offset(buf_offset, segment_data);

						if written < segment_data.len() {
							warn!("Write buffer exhausted, could write only {} bytes of {} at offset = {}, global_offset = {}", written, segment_data.len(), buf_offset, self.buffer_start_offset);
						}
					}
				}
				_ => {
					// TODO might have handled some chunks, how to report?
					return Err(Error::new(
						ErrorKind::InvalidData,
						format!("Unknown chunk type {} with size {}", chunk_type, chunk_size),
					));
				}
			}
		}

		Ok(rd.position() as usize)
	}

	pub fn gen_feedback_packet(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		//for missing in self.fragments.missing() {}
		unimplemented!()
	}
}

impl Read for Receiver {
	fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		unimplemented!();
	}
}

#[cfg(test)]
mod received_tests {
	use super::*;

	// #[test]
	// fn basic_packet_parse() {
	// 	let mut recv = Receiver::new(4);
	// 	let mut buf = [0u8; 32];
	// 	let mut c = Cursor::new(&mut buf[..]);
	//
	// 	let data = &b"SHLANG"[..];
	// 	c.write_u8((data.len() - 1) as u8).unwrap();
	// 	c.write(&data).unwrap();
	//
	// 	c.write_u8(7).unwrap();
	// 	c.write_u32::<LittleEndian>(0).unwrap();
	// 	c.write_u32::<LittleEndian>(0).unwrap();
	//
	// 	let pos = c.position() as usize;
	// 	assert_eq!(recv.receive_packet(&buf[..pos]).unwrap(), data.len());
	//
	// 	let mut buf = [0u8; 32];
	// 	assert_eq!(recv.read(&mut buf).unwrap(), data.len());
	// 	assert_eq!(&buf[..data.len()], data);
	// }
	//
	// #[test]
	// fn basic_packet_2fragments_parse() {
	// 	let mut recv = Receiver::new(4);
	// 	let mut buf = [0u8; 32];
	// 	let mut c = Cursor::new(&mut buf[..]);
	//
	// 	let data1 = &b"SHLANG"[..];
	// 	c.write_u8((data1.len() - 1) as u8).unwrap();
	// 	c.write(&data1).unwrap();
	//
	// 	let data2 = &b"TPOC"[..];
	// 	c.write_u8((data2.len() - 1) as u8).unwrap();
	// 	c.write(&data2).unwrap();
	//
	// 	c.write_u8(3 * 4 - 1).unwrap();
	// 	c.write_u32::<LittleEndian>(0).unwrap();
	// 	c.write_u32::<LittleEndian>(0).unwrap();
	// 	c.write_u32::<LittleEndian>(1).unwrap();
	//
	// 	let pos = c.position() as usize;
	// 	assert_eq!(
	// 		recv.receive_packet(&buf[..pos]).unwrap(),
	// 		data1.len() + data2.len()
	// 	);
	//
	// 	let mut buf = [0u8; 32];
	// 	assert_eq!(recv.read(&mut buf).unwrap(), data1.len() + data2.len());
	// 	assert_eq!(&buf[..data1.len()], data1);
	// 	assert_eq!(&buf[data1.len()..data1.len() + data2.len()], data2);
	// }
	//
	// #[test]
	// fn basic_packet_read_split() {
	// 	let mut recv = Receiver::new(4);
	// 	let mut buf = [0u8; 32];
	// 	let mut c = Cursor::new(&mut buf[..]);
	//
	// 	let data = &b"SHLANG"[..];
	// 	c.write_u8((data.len() - 1) as u8).unwrap();
	// 	c.write(&data).unwrap();
	//
	// 	c.write_u8(7).unwrap();
	// 	c.write_u32::<LittleEndian>(0).unwrap();
	// 	c.write_u32::<LittleEndian>(0).unwrap();
	//
	// 	let pos = c.position() as usize;
	// 	assert_eq!(recv.receive_packet(&buf[..pos]).unwrap(), data.len());
	//
	// 	let mut buf = [0u8; 3];
	// 	assert_eq!(recv.read(&mut buf).unwrap(), 3);
	// 	assert_eq!(buf, data[..3]);
	//
	// 	let mut buf = [0u8; 2];
	// 	assert_eq!(recv.read(&mut buf).unwrap(), 2);
	// 	assert_eq!(buf, data[3..5]);
	//
	// 	let mut buf = [0u8; 32];
	// 	assert_eq!(recv.read(&mut buf).unwrap(), 1);
	// 	assert_eq!(buf[..1], data[5..]);
	// }
}
