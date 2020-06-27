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
		unimplemented!();
		// if self.read <= self.write {
		// 	// let dst = &mut self.buf[self.read..self.write];
		// 	// let size = min(dst.len(), data.len());
		// 	// return size;
		// } else {
		// }
	}

	pub fn get_data(&self) -> [&[u8]; 2] {
		unimplemented!();
	}

	pub fn consume_back(&mut self, offset: usize) {
		// -> std::io::Result<()> {
		self.read = (self.read + offset) % self.buf.len();
	}
}
