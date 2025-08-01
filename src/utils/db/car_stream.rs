// Copyright 2019-2025 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

use crate::db::car::plain::read_v2_header;
use crate::utils::multihash::prelude::*;
use async_compression::tokio::bufread::ZstdDecoder;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use cid::Cid;
use futures::ready;
use futures::{Stream, StreamExt, sink::Sink};
use fvm_ipld_encoding::to_vec;
use integer_encoding::VarInt;
use nunny::Vec as NonEmpty;
use pin_project_lite::pin_project;
use serde::{Deserialize, Serialize};
use std::io::{self, SeekFrom};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, AsyncWrite,
    Take,
};
use tokio_util::codec::Encoder;
use tokio_util::codec::FramedRead;
use tokio_util::either::Either;
use unsigned_varint::codec::UviBytes;

use crate::utils::encoding::from_slice_with_fallback;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CarV1Header {
    // The roots array must contain one or more CIDs,
    // each of which should be present somewhere in the remainder of the CAR.
    // See <https://ipld.io/specs/transport/car/carv1/#constraints>
    pub roots: NonEmpty<Cid>,
    pub version: u64,
}

/// <https://ipld.io/specs/transport/car/carv2/#header>
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CarV2Header {
    pub characteristics: [u8; 16],
    pub data_offset: i64,
    pub data_size: i64,
    pub index_offset: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CarBlock {
    pub cid: Cid,
    pub data: Vec<u8>,
}

impl CarBlock {
    // Write a varint frame containing the cid and the data
    pub fn write(&self, mut writer: &mut impl io::Write) -> io::Result<()> {
        let frame_length = self.cid.encoded_len() + self.data.len();
        writer.write_all(&frame_length.encode_var_vec())?;
        #[allow(clippy::needless_borrows_for_generic_args)]
        self.cid
            .write_bytes(&mut writer)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        writer.write_all(&self.data)?;
        Ok(())
    }

    pub fn from_bytes(bytes: impl Into<Bytes>) -> io::Result<CarBlock> {
        let bytes: Bytes = bytes.into();
        let mut cursor = bytes.reader();
        let cid = Cid::read_bytes(&mut cursor)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let bytes = cursor.into_inner();
        Ok(CarBlock {
            cid,
            data: bytes.to_vec(),
        })
    }

    pub fn valid(&self) -> bool {
        self.validate().is_ok()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let actual = {
            let code = MultihashCode::try_from(self.cid.hash().code())?;
            Cid::new_v1(self.cid.codec(), code.digest(&self.data))
        };
        anyhow::ensure!(
            actual == self.cid,
            "CID/Block mismatch for block {}, actual: {actual}",
            self.cid
        );
        Ok(())
    }
}

pin_project! {
    /// Stream of CAR blocks. If the input data is compressed with zstd, it will
    /// automatically be decompressed.
    pub struct CarStream<ReaderT> {
        #[pin]
        reader: FramedRead<Take<Either<ReaderT, ZstdDecoder<ReaderT>>>, UviBytes>,
        pub header_v1: CarV1Header,
        pub header_v2: Option<CarV2Header>,
        first_block: Option<CarBlock>,
    }
}

// This method checks the header in order to see whether or not we are operating on a zstd
// archive. The zstd header has a maximum size of 18 bytes:
// https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#zstandard-frames.
fn is_zstd(buf: &[u8]) -> bool {
    zstd::zstd_safe::get_frame_content_size(buf).is_ok()
}

impl<ReaderT: AsyncBufRead + Unpin> CarStream<ReaderT> {
    /// Create a stream with automatic but unsafe CARv2 header extraction.
    ///
    /// Note that if the input is zstd compressed, the CARv2 header extraction
    /// is on a best efforts basis. It could fail when `reader.fill_buf()` is insufficient
    /// for decoding the first zstd frame, and treat input as CARv1, because this method
    /// does not require the input to be [`tokio::io::AsyncSeek`].
    /// It's recommended to use [`CarStream::new`] for zstd compressed CARv2 input.
    #[allow(dead_code)]
    pub async fn new_unsafe(mut reader: ReaderT) -> io::Result<Self> {
        let header_v2 = Self::try_decode_header_v2_from_fill_buf(reader.fill_buf().await?)
            // treat input as CARv1 if zstd decoding failed
            .ok()
            .flatten();
        Self::new_with_header_v2(reader, header_v2).await
    }

    /// Create a stream with pre-extracted CARv2 header
    pub async fn new_with_header_v2(
        mut reader: ReaderT,
        header_v2: Option<CarV2Header>,
    ) -> io::Result<Self> {
        let is_compressed = is_zstd(reader.fill_buf().await?);
        let mut reader = if is_compressed {
            let mut zstd = ZstdDecoder::new(reader);
            zstd.multiple_members(true);
            Either::Right(zstd)
        } else {
            Either::Left(reader)
        };

        // Skip v2 header bytes
        if let Some(header_v2) = &header_v2 {
            let mut to_skip = vec![0; header_v2.data_offset as usize];
            reader.read_exact(&mut to_skip).await?;
        }

        let max_car_v1_bytes = header_v2
            .as_ref()
            .map(|h| h.data_size as u64)
            .unwrap_or(u64::MAX);
        let mut reader = FramedRead::new(reader.take(max_car_v1_bytes), UviBytes::default());
        let header_v1 = read_v1_header(&mut reader)
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid v1 header block"))?;

        // Read the first block and check if it is valid. This check helps to
        // catch invalid CAR files as soon as we open.
        if let Some(first_entry) = reader.next().await.transpose()? {
            let block = CarBlock::from_bytes(first_entry)?;
            if !block.valid() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid first block",
                ));
            }
            Ok(CarStream {
                reader,
                header_v1,
                header_v2,
                first_block: Some(block),
            })
        } else {
            Ok(CarStream {
                reader,
                header_v1,
                header_v2,
                first_block: None,
            })
        }
    }

    /// Extracts CARv2 header from the input, returns the reader and CARv2 header.
    ///
    /// Note that position of the input reader has to be reset before calling [`CarStream::new_with_header_v2`].
    /// Use [`CarStream::extract_header_v2_and_reset_reader_position`] to automatically reset stream position.
    pub async fn extract_header_v2(
        mut reader: ReaderT,
    ) -> io::Result<(ReaderT, Option<CarV2Header>)> {
        let is_compressed = is_zstd(reader.fill_buf().await?);
        let mut reader = if is_compressed {
            let mut zstd = ZstdDecoder::new(reader);
            zstd.multiple_members(true);
            Either::Right(zstd)
        } else {
            Either::Left(reader)
        };
        let mut possible_header_bytes = [0; 51];
        reader.read_exact(&mut possible_header_bytes).await?;
        let header_v2 = read_v2_header(possible_header_bytes.as_slice())?;
        let reader = match reader {
            Either::Left(reader) => reader,
            Either::Right(zstd) => zstd.into_inner(),
        };
        Ok((reader, header_v2))
    }

    fn try_decode_header_v2_from_fill_buf(fill_buf: &[u8]) -> io::Result<Option<CarV2Header>> {
        let is_compressed = is_zstd(fill_buf);
        let fill_buf_reader = if is_compressed {
            itertools::Either::Right(zstd::Decoder::new(fill_buf)?)
        } else {
            itertools::Either::Left(fill_buf)
        };
        read_v2_header(fill_buf_reader)
    }
}

impl<ReaderT: AsyncBufRead + AsyncSeek + Unpin> CarStream<ReaderT> {
    /// Create a stream with automatic CARv2 header extraction.
    pub async fn new(reader: ReaderT) -> io::Result<Self> {
        let (reader, header_v2) = Self::extract_header_v2_and_reset_reader_position(reader).await?;
        Self::new_with_header_v2(reader, header_v2).await
    }

    /// Extracts CARv2 header from the input, resets the reader position and returns the reader and CARv2 header.
    pub async fn extract_header_v2_and_reset_reader_position(
        mut reader: ReaderT,
    ) -> io::Result<(ReaderT, Option<CarV2Header>)> {
        let stream_position = reader.stream_position().await?;
        let (mut reader, header_v2) = Self::extract_header_v2(reader).await?;
        reader.seek(SeekFrom::Start(stream_position)).await?;
        Ok((reader, header_v2))
    }
}

impl<ReaderT: AsyncBufRead> Stream for CarStream<ReaderT> {
    type Item = io::Result<CarBlock>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        if let Some(block) = this.first_block.take() {
            return Poll::Ready(Some(Ok(block)));
        }
        let item = futures::ready!(this.reader.poll_next(cx));
        Poll::Ready(item.map(|ret| ret.and_then(CarBlock::from_bytes)))
    }
}

pin_project! {
    pub struct CarWriter<W> {
        #[pin]
        inner: W,
        buffer: BytesMut,
    }
}

impl<W: AsyncWrite> CarWriter<W> {
    pub fn new_carv1(roots: NonEmpty<Cid>, writer: W) -> io::Result<Self> {
        let car_header = CarV1Header { roots, version: 1 };

        let mut header_uvi_frame = BytesMut::new();
        UviBytes::default().encode(Bytes::from(to_vec(&car_header)?), &mut header_uvi_frame)?;

        Ok(Self {
            inner: writer,
            buffer: header_uvi_frame,
        })
    }
}

impl<W: AsyncWrite> Sink<CarBlock> for CarWriter<W> {
    type Error = io::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut this = self.as_mut().project();

        while !this.buffer.is_empty() {
            this = self.as_mut().project();
            let bytes_written = ready!(this.inner.poll_write(cx, this.buffer))?;
            this.buffer.advance(bytes_written);
        }
        Poll::Ready(Ok(()))
    }
    fn start_send(self: Pin<&mut Self>, item: CarBlock) -> Result<(), Self::Error> {
        item.write(&mut self.project().buffer.writer())
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().poll_ready(cx))?;
        self.project().inner.poll_flush(cx)
    }
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().poll_ready(cx))?;
        self.project().inner.poll_shutdown(cx)
    }
}

async fn read_v1_header<ReaderT: AsyncRead + Unpin>(
    framed_reader: &mut FramedRead<ReaderT, UviBytes>,
) -> Option<CarV1Header> {
    let frame = framed_reader.next().await?.ok()?;
    let header = from_slice_with_fallback::<CarV1Header>(&frame).ok()?;
    if header.version != 1 {
        return None;
    }
    Some(header)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::networks::{calibnet, mainnet};
    use futures::TryStreamExt;
    use quickcheck::{Arbitrary, Gen};

    impl Arbitrary for CarBlock {
        fn arbitrary(g: &mut Gen) -> CarBlock {
            let data = Vec::<u8>::arbitrary(g);
            let encoding = g
                .choose(&[
                    fvm_ipld_encoding::DAG_CBOR,
                    fvm_ipld_encoding::CBOR,
                    fvm_ipld_encoding::IPLD_RAW,
                ])
                .unwrap();
            let code = g
                .choose(&[MultihashCode::Blake2b256, MultihashCode::Sha2_256])
                .unwrap();
            let cid = Cid::new_v1(*encoding, code.digest(&data));
            CarBlock { cid, data }
        }
    }

    #[tokio::test]
    async fn stream_calibnet_genesis_unsafe() {
        let stream = CarStream::new_unsafe(calibnet::DEFAULT_GENESIS)
            .await
            .unwrap();
        let blocks: Vec<CarBlock> = stream.try_collect().await.unwrap();
        assert_eq!(blocks.len(), 1207);
        for block in blocks {
            block.validate().unwrap();
        }
    }

    #[tokio::test]
    async fn stream_calibnet_genesis() {
        let stream = CarStream::new(Cursor::new(calibnet::DEFAULT_GENESIS))
            .await
            .unwrap();
        let blocks: Vec<CarBlock> = stream.try_collect().await.unwrap();
        assert_eq!(blocks.len(), 1207);
        for block in blocks {
            block.validate().unwrap();
        }
    }

    #[tokio::test]
    async fn stream_mainnet_genesis_unsafe() {
        let stream = CarStream::new_unsafe(mainnet::DEFAULT_GENESIS)
            .await
            .unwrap();
        let blocks: Vec<CarBlock> = stream.try_collect().await.unwrap();
        assert_eq!(blocks.len(), 1222);
        for block in blocks {
            block.validate().unwrap();
        }
    }

    #[tokio::test]
    async fn stream_mainnet_genesis() {
        let stream = CarStream::new(Cursor::new(mainnet::DEFAULT_GENESIS))
            .await
            .unwrap();
        let blocks: Vec<CarBlock> = stream.try_collect().await.unwrap();
        assert_eq!(blocks.len(), 1222);
        for block in blocks {
            block.validate().unwrap();
        }
    }
}
