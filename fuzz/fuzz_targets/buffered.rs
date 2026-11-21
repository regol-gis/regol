use regol::source::Source;

/// Force iterator-owned buffers instead of borrowing the input slice.
pub struct Buffered<'a> {
    pub bytes: &'a [u8],
    pub width: usize,
}
impl Source for Buffered<'_> {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }
    fn read_buffer_size(&self) -> usize {
        self.width
    }
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> std::io::Result<()> {
        self.bytes.read_exact_at(offset, output)
    }
}
