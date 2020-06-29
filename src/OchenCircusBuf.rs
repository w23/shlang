use std::cmp::min;
pub struct OchenCircusBuf {
	buf: Box<[u8]>,
	read: usize,
	write: usize,
}

impl OchenCircusBuf {
	pub fn with_capacity(capacity: usize) -> Self {
		Self {
			buf: vec![0; capacity].into_boxed_slice(),
			read: 0,
			write: 0,
		}
	}

	pub fn len(&self) -> usize {
		// read == write => empty
		if self.read <= self.write {
			self.write - self.read
		} else {
			self.buf.len() - self.read + self.write
		}
	}

	pub fn write_data_at_read_offset(&mut self, offset: usize, data: &[u8]) -> usize {
		if offset >= self.buf.len() {
			return 0;
		}

		let write_size = min(self.buf.len() - offset - 1, data.len());
		let write_pos = (self.read + offset) % self.buf.len();

		let chunk_size = min(write_size, self.buf.len() - write_pos);
		self.buf[write_pos..write_pos + chunk_size].clone_from_slice(&data[..chunk_size]);

		let data = &data[chunk_size..];
		if data.len() > 0 {
			self.buf[..data.len()].clone_from_slice(data);
		}

		self.write = (write_pos + write_size) % self.buf.len();
		write_size
	}

	pub fn get_data(&self) -> [&[u8]; 2] {
		if self.read <= self.write {
			[&self.buf[self.read..self.write], &[]]
		} else {
			[&self.buf[self.read..], &self.buf[..self.write]]
		}
	}

	pub fn consume(&mut self, offset: usize) {
		// -> std::io::Result<()> {
		self.read = (self.read + offset) % self.buf.len();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn basic_write_read() {
		let mut buf = OchenCircusBuf::with_capacity(16);
		let data = &b"KEQUE"[..];
		assert_eq!(buf.write_data_at_read_offset(0, &data), data.len());
		let [buf1, buf2] = buf.get_data();
		assert_eq!(buf1, data);
		assert_eq!(buf2.len(), 0);

		buf.consume(data.len());
		assert_eq!(buf.len(), 0);
		assert_eq!(buf.get_data(), [[], []]);
	}

	#[test]
	fn write_read_at_offset() {
		let mut buf = OchenCircusBuf::with_capacity(16);
		let data = &b"KEQUE"[..];
		assert_eq!(buf.write_data_at_read_offset(10, &data), data.len());
		let [buf1, buf2] = buf.get_data();
		assert_eq!(buf1.len() - 10, data.len());
		assert_eq!(&buf1[10..], data);
		assert_eq!(buf2.len(), 0);

		buf.consume(data.len() + 10);
		assert_eq!(buf.len(), 0);
		assert_eq!(buf.get_data(), [[], []]);
	}

	#[test]
	fn write_read_at_offset_wrapped() {
		let mut buf = OchenCircusBuf::with_capacity(16);
		let data = &b"KEQUE"[..];
		assert_eq!(buf.write_data_at_read_offset(5, &data), data.len());
		// .....KEQUE______

		let [buf1, buf2] = buf.get_data();
		assert_eq!(buf1.len() - 5, data.len());
		assert_eq!(&buf1[5..], data);
		assert_eq!(buf2.len(), 0);

		buf.consume(8);
		// ________UE______
		assert_eq!(buf.len(), 2);
		assert_eq!(buf.get_data(), [&data[3..], &[]]);

		let data2 = &b"P0GGERS"[..];
		assert_eq!(buf.write_data_at_read_offset(4, &data2), data2.len());
		// ERS_____UE..P0GG
		//
		let [buf1, buf2] = buf.get_data();
		assert_eq!(&buf1[..2], &data[3..]);
		assert_eq!(&buf1[4..], &data2[..4]);
		assert_eq!(&buf2[..3], &data2[4..]);

		buf.consume(9);
		// _RS_____________
		assert_eq!(buf.len(), 2);
		assert_eq!(buf.get_data(), [&data2[5..], &[]]);
	}
}
