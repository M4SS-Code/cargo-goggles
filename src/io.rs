use std::io::{self, BufRead, Read};

pub struct AsciiWhitespaceSkippingReader<R>(R);

impl<R> AsciiWhitespaceSkippingReader<R> {
    pub fn new(reader: R) -> Self {
        Self(reader)
    }
}

impl<R> Read for AsciiWhitespaceSkippingReader<R>
where
    R: BufRead,
{
    fn read(&mut self, mut buf: &mut [u8]) -> io::Result<usize> {
        let mut written = 0;

        loop {
            if buf.is_empty() {
                break;
            }

            let mut read_buf = self.0.fill_buf()?;
            if read_buf.is_empty() {
                break;
            }

            let mut read = 0;
            while !read_buf.is_empty() && !buf.is_empty() {
                read += 1;
                let b = read_buf[0];
                read_buf = &read_buf[1..];
                if b.is_ascii_whitespace() {
                    continue;
                }

                buf[0] = b;
                buf = &mut buf[1..];
                written += 1;
            }

            self.0.consume(read);
        }

        Ok(written)
    }
}
