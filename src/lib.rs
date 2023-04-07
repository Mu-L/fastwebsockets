// Copyright 2023 Divy Srivastava <dj.srivastava23@gmail.com>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod close;
mod fragment;
mod frame;
mod mask;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

pub use crate::close::CloseCode;
pub use crate::fragment::FragmentCollector;
pub use crate::frame::Frame;
pub use crate::frame::OpCode;
pub use crate::mask::unmask;

pub struct WebSocket<S> {
  stream: S,
  write_buffer: Vec<u8>,
  partial_write: Option<Vec<u8>>,
  read_buffer: Option<Vec<u8>>,
  vectored: bool,
  auto_close: bool,
  auto_pong: bool,
  max_message_size: usize,
}

impl<S> WebSocket<S> {
  pub fn try_write(
    &mut self,
    mut frame: Frame,
    cb: impl FnOnce(&mut S, &[u8]) -> std::io::Result<usize>,
  ) -> bool {
    assert!(self.partial_write.is_none()); // There should be no partial write in progress

    let text = frame.write(&mut self.write_buffer);
    let written = cb(&mut self.stream, text).unwrap_or(0);
    // Not the most optimal approach, but this is the slow path anyway.
    if written < text.len() {
      self.partial_write = Some(text[written..].to_vec());
    }

    self.partial_write.is_none()
  }
}

impl<S> WebSocket<S> {
  pub fn after_handshake(stream: S) -> Self
  where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
  {
    Self {
      stream,
      write_buffer: Vec::with_capacity(2),
      read_buffer: None,
      vectored: false,
      auto_close: true,
      auto_pong: true,
      partial_write: None,
      max_message_size: 64 << 20,
    }
  }

  pub fn set_writev(&mut self, vectored: bool) {
    self.vectored = vectored;
  }

  pub fn set_auto_close(&mut self, auto_close: bool) {
    self.auto_close = auto_close;
  }

  pub fn set_auto_pong(&mut self, auto_pong: bool) {
    self.auto_pong = auto_pong;
  }

  pub fn set_max_message_size(&mut self, max_message_size: usize) {
    self.max_message_size = max_message_size;
  }

  pub async fn write_frame(
    &mut self,
    mut frame: Frame,
  ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
  where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
  {
    if let Some(partial) = self.partial_write.take() {
      self.stream.write_all(&partial).await?;
      return Ok(());
    }

    if self.vectored {
      frame.writev(&mut self.stream).await?;
    } else {
      let text = frame.write(&mut self.write_buffer);
      self.stream.write_all(text).await?;
    }

    Ok(())
  }

  pub async fn read_frame(
    &mut self,
  ) -> Result<Frame, Box<dyn std::error::Error + Send + Sync>>
  where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
  {
    loop {
      let mut frame = self.parse_frame_header().await?;
      frame.unmask();

      match frame.opcode {
        OpCode::Close if self.auto_close => {
          match frame.payload.len() {
            0 => {}
            1 => return Err("invalid close frame".into()),
            _ => {
              let code = close::CloseCode::from(u16::from_be_bytes(
                frame.payload[0..2].try_into().unwrap(),
              ));

              #[cfg(feature = "simd")]
              simdutf8::basic::from_utf8(&frame.payload[2..])?;

              #[cfg(not(feature = "simd"))]
              std::str::from_utf8(&frame.payload[2..])?;

              if !code.is_allowed() {
                self
                  .write_frame(Frame::close(1002, &frame.payload[2..]))
                  .await?;

                return Err("invalid close code".into());
              }
            }
          };

          self
            .write_frame(Frame::close_raw(frame.payload.clone()))
            .await?;
          break Ok(frame);
        }
        OpCode::Ping if self.auto_pong => {
          self.write_frame(Frame::pong(frame.payload)).await?;
        }
        OpCode::Text => {
          if frame.fin && !frame.is_utf8() {
            break Err("invalid utf-8".into());
          }

          break Ok(frame);
        }
        OpCode::Pong => {}
        _ => break Ok(frame),
      }
    }
  }

  async fn parse_frame_header(
    &mut self,
  ) -> Result<Frame, Box<dyn std::error::Error + Send + Sync>>
  where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
  {
    let mut head = [0; 2 + 4 + 100];

    let mut nread = 0;

    if let Some(buffer) = self.read_buffer.take() {
      head[..buffer.len()].copy_from_slice(&buffer);
      nread = buffer.len();
    }

    while nread < 2 {
      nread += self.stream.read(&mut head[nread..]).await?;
    }

    let fin = head[0] & 0b10000000 != 0;

    let rsv1 = head[0] & 0b01000000 != 0;
    let rsv2 = head[0] & 0b00100000 != 0;
    let rsv3 = head[0] & 0b00010000 != 0;

    if rsv1 || rsv2 || rsv3 {
      return Err("reserved bits are not zero".into());
    }

    let opcode = frame::OpCode::try_from(head[0] & 0b00001111)?;
    let masked = head[1] & 0b10000000 != 0;

    let length_code = head[1] & 0x7F;
    let extra = match length_code {
      126 => 2,
      127 => 8,
      _ => 0,
    };

    let length: usize = if extra > 0 {
      while nread < 2 + extra {
        nread += self.stream.read(&mut head[nread..]).await?;
      }

      match extra {
        2 => u16::from_be_bytes(head[2..4].try_into().unwrap()) as usize,
        8 => usize::from_be_bytes(head[2..10].try_into().unwrap()),
        _ => unreachable!(),
      }
    } else {
      usize::from(length_code)
    };

    let mask = match masked {
      true => {
        while nread < 2 + extra + 4 {
          nread += self.stream.read(&mut head[nread..]).await?;
        }

        Some(head[2 + extra..2 + extra + 4].try_into().unwrap())
      }
      false => None,
    };

    if frame::is_control(opcode) && !fin {
      return Err("control frame must not be fragmented".into());
    }

    if opcode == OpCode::Ping && length > 125 {
      return Err("Ping frame too large".into());
    }

    if length >= self.max_message_size {
      return Err("Frame too large".into());
    }

    let required = 2 + extra + mask.map(|_| 4).unwrap_or(0) + length;

    if required > nread {
      // Allocate more space
      let mut new_head = head.to_vec();
      new_head.resize(required, 0);

      self.stream.read_exact(&mut new_head[nread..]).await?;

      return Ok(Frame::new(
        fin,
        opcode,
        mask,
        new_head[required - length..].to_vec(),
      ));
    } else if nread > required {
      // We read too much
      self.read_buffer = Some(head[required..nread].to_vec());
    }

    Ok(Frame::new(
      fin,
      opcode,
      mask,
      head[required - length..required].to_vec(),
    ))
  }
}
