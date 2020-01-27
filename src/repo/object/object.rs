/*
 * Copyright 2019 Garrett Powell
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::cmp::min;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::mem::replace;

use blake2::digest::{Input, VariableOutput};
use blake2::VarBlake2b;
use cdchunking::ZPAQ;
use serde::{Deserialize, Serialize};

use crate::store::DataStore;

use super::chunking::IncrementalChunker;
use super::header::Key;
use super::repository::ObjectRepository;

/// The size of the checksums used for uniquely identifying chunks.
pub const CHUNK_HASH_SIZE: usize = 32;

/// A 256-bit checksum used for uniquely identifying a chunk.
pub type ChunkHash = [u8; CHUNK_HASH_SIZE];

/// Compute the BLAKE2 checksum of the given `data` and return the result.
pub fn chunk_hash(data: &[u8]) -> ChunkHash {
    let mut hasher = VarBlake2b::new(CHUNK_HASH_SIZE).unwrap();
    hasher.input(data);
    let mut checksum = [0u8; CHUNK_HASH_SIZE];
    hasher.variable_result(|result| checksum.copy_from_slice(result));
    checksum
}

/// A chunk of data generated by the chunking algorithm.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Chunk {
    /// The size of the chunk in bytes.
    pub size: usize,

    /// The checksum of the chunk.
    pub hash: ChunkHash,
}

/// The location of a chunk in a stream of bytes.
#[derive(Debug, PartialEq, Eq, Clone, Default)]
struct ChunkLocation {
    /// The chunk itself.
    pub chunk: Chunk,

    /// The offset of the start of the chunk from the beginning of the object.
    pub start: u64,

    /// The offset of the end of the chunk from the beginning of the object.
    pub end: u64,

    /// The offset of the seek position from the beginning of the object.
    pub position: u64,

    /// The index of the chunk in the list of chunks.
    pub index: usize,
}

impl ChunkLocation {
    /// The offset of the seek position from the beginning of the chunk.
    fn relative_position(&self) -> usize {
        (self.position - self.start) as usize
    }
}

/// A handle for accessing data in a repository.
///
/// An `Object` doesn't own or store data itself, but references data stored in a repository.
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct ObjectHandle {
    /// The original size of the data in bytes.
    pub size: u64,

    /// The checksums of the chunks which make up the data.
    pub chunks: Vec<Chunk>,
}

impl Default for ObjectHandle {
    fn default() -> Self {
        Self {
            size: 0,
            chunks: Vec::new(),
        }
    }
}

/// A value that uniquely identifies the contents of an object.
///
/// A `ContentId` is like a checksum of the data in an object except it is cheap to compute.
/// A `ContentId` can be compared with other `ContentId` values to determine if the contents of two
/// objects are equal. However, these comparisons are only valid within the same repository;
/// comparisons between `ContentId`s from different repositories are meaningless. To compare data
/// between repositories, an actual checksum of the data must be used.
///
/// `ContentId` is opaque, but it can be serialized and deserialized. The value of a `ContentId` is
/// valid for the lifetime of a repository, meaning that they can be compared across invocations of
/// the library.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy, Serialize, Deserialize)]
pub struct ContentId([u8; 32]);

/// A handle for accessing data in a repository.
///
/// An `Object` represents the data associated with a key in an `ObjectRepository`. It implements
/// `Read`, `Write`, and `Seek` for reading data from the repository and writing data to the
/// repository.
///
/// Because `Object` internally buffers data when reading, there's no need to use a buffered reader
/// like `BufReader`.
///
/// When writing to the object, `flush` must be called explicitly. When the object is dropped or
/// `seek` or `truncate` is called, any unflushed data is discarded. The object's `size` and
/// `content_id` are not updated until `flush` is called, and `verify` will not verify the integrity
/// of unflushed data.
///
/// If encryption is enabled for the repository, data integrity is automatically verified as it is
/// read and methods will return an `Err` if corrupt data is found. The `verify` method can be used
/// to check the integrity of all the data in the object whether encryption is enabled or not.
///
/// The methods of `Read`, `Write`, and `Seek` return `io::Result`, but the returned `io::Error` can
/// be converted `Into` a `data_store::Error` to be consistent with the rest of the library. The
/// implementations document which `data_store::Error` values can be returned.
pub struct Object<'a, K: Key, S: DataStore> {
    /// The repository where chunks are stored.
    repository: &'a mut ObjectRepository<K, S>,

    /// The key which represents this object.
    key: K,

    /// An object responsible for buffering and chunking data which has been written.
    chunker: IncrementalChunker<ZPAQ>,

    /// The list of chunks which have been written since `flush` was last called.
    new_chunks: Vec<Chunk>,

    /// The location of the first chunk written to since `flush` was last called.
    ///
    /// If no data has been written, this is `None`.
    start_location: Option<ChunkLocation>,

    /// The current seek position of the object.
    position: u64,

    /// The chunk which was most recently read from.
    ///
    /// If no data has been read, this is `None`.
    buffered_chunk: Option<Chunk>,

    /// The contents of the chunk which was most recently read from.
    read_buffer: Vec<u8>,
}

impl<'a, K: Key, S: DataStore> Object<'a, K, S> {
    pub(super) fn new(
        repository: &'a mut ObjectRepository<K, S>,
        key: K,
        chunker_bits: usize,
    ) -> Self {
        Self {
            repository,
            key,
            chunker: IncrementalChunker::new(ZPAQ::new(chunker_bits)),
            new_chunks: Vec::new(),
            start_location: None,
            position: 0,
            buffered_chunk: None,
            read_buffer: Vec::new(),
        }
    }

    /// Get the object handle for the object associated with `key`.
    fn get_handle(&self) -> &ObjectHandle {
        self.repository.get_handle(&self.key)
    }

    /// Get the object handle for the object associated with `key`.
    fn get_handle_mut(&mut self) -> &mut ObjectHandle {
        self.repository.get_handle_mut(&self.key)
    }

    /// Return the size of the object in bytes.
    pub fn size(&self) -> u64 {
        self.repository.get_handle(&self.key).size
    }

    /// Return a `ContentId` representing the contents of this object.
    ///
    /// The returned value represents the contents of the object at the time this method was called.
    pub fn content_id(&self) -> ContentId {
        // The content ID is just a hash of all the chunk hashes, which is cheap to compute.
        let mut concatenation = Vec::new();
        for chunk in &self.get_handle().chunks {
            concatenation.extend_from_slice(&chunk.hash);
        }
        ContentId(chunk_hash(concatenation.as_slice()))
    }

    /// Verify the integrity of the data in this object.
    ///
    /// This returns `true` if the object is valid and `false` if it is corrupt.
    ///
    /// # Errors
    /// - `Error::InvalidData`: Ciphertext verification failed.
    /// - `Error::Store`: An error occurred with the data store.
    /// - `Error::Io`: An I/O error occurred.
    pub fn verify(&self) -> crate::Result<bool> {
        for expected_chunk in &self.get_handle().chunks {
            match self.repository.read_chunk(&expected_chunk.hash) {
                Ok(data) => {
                    let actual_checksum = chunk_hash(&data);
                    if expected_chunk.hash != actual_checksum {
                        return Ok(false);
                    }
                }
                Err(crate::Error::InvalidData) => return Ok(false),
                Err(error) => return Err(error),
            }
        }

        Ok(true)
    }

    /// Truncate the object to the given `length`.
    ///
    /// If the given `length` is greater than or equal to the current size of the object, this does
    /// nothing. If the seek position is past the point which the object is truncated to, it is
    /// moved to the new end of the object.
    ///
    /// # Errors
    /// - `Error::InvalidData`: Ciphertext verification failed.
    /// - `Error::Store`: An error occurred with the data store.
    /// - `Error::Io`: An I/O error occurred.
    pub fn truncate(&mut self, length: u64) -> crate::Result<()> {
        // Clear all written data which has not been flushed.
        self.new_chunks.clear();
        self.chunker.clear();

        if length >= self.get_handle().size {
            return Ok(());
        }

        let original_position = self.position;
        self.position = length;

        // Truncating the object may mean slicing a chunk in half. Because we can't edit chunks
        // in-place, we need to read the final chunk, slice it, and write it back.
        let end_location = match self.current_chunk() {
            Some(location) => location,
            None => return Ok(()),
        };
        let last_chunk = self.repository.read_chunk(&end_location.chunk.hash)?;
        let new_last_chunk = &last_chunk[..end_location.relative_position()];
        let new_last_chunk = Chunk {
            hash: self.repository.write_chunk(&new_last_chunk)?,
            size: new_last_chunk.len(),
        };

        // Remove all chunks including and after the final chunk.
        self.get_handle_mut().chunks.drain(end_location.index..);

        // Append the new final chunk which has been sliced.
        self.get_handle_mut().chunks.push(new_last_chunk);

        // Update the object size.
        let current_size = self.get_handle().size;
        self.get_handle_mut().size = min(length, current_size);

        // Restore the seek position.
        self.position = min(original_position, length);

        Ok(())
    }

    /// Return the chunk at the current seek position or `None` if there is none.
    fn current_chunk(&self) -> Option<ChunkLocation> {
        let mut chunk_start = 0u64;
        let mut chunk_end = 0u64;

        for (index, chunk) in self.get_handle().chunks.iter().enumerate() {
            chunk_end += chunk.size as u64;
            if self.position >= chunk_start && self.position < chunk_end {
                return Some(ChunkLocation {
                    chunk: *chunk,
                    start: chunk_start,
                    end: chunk_end,
                    position: self.position,
                    index,
                });
            }
            chunk_start += chunk.size as u64;
        }

        // There are no chunks in the object.
        None
    }

    /// Return the slice of bytes between the current seek position and the end of the chunk.
    ///
    /// The returned slice will be no longer than `size`.
    fn read_chunk(&mut self, size: usize) -> crate::Result<&[u8]> {
        // If the object is empty, there's no data to read.
        let current_location = match self.current_chunk() {
            Some(location) => location,
            None => return Ok(&[]),
        };

        // If we're reading from a new chunk, read the contents of that chunk into the read buffer.
        if Some(current_location.chunk) != self.buffered_chunk {
            self.buffered_chunk = Some(current_location.chunk);
            self.read_buffer = self.repository.read_chunk(&current_location.chunk.hash)?;
        }

        let start = current_location.relative_position();
        let end = min(start + size, current_location.chunk.size as usize);
        Ok(&self.read_buffer[start..end])
    }

    /// Write chunks stored in the chunker to the repository.
    fn write_chunks(&mut self) -> crate::Result<()> {
        for chunk in self.chunker.chunks() {
            let hash = self.repository.write_chunk(&chunk)?;
            self.new_chunks.push(Chunk {
                hash,
                size: chunk.len(),
            });
        }
        Ok(())
    }
}

impl<'a, K: Key, S: DataStore> Seek for Object<'a, K, S> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        // Clear all written data which has not been flushed.
        self.new_chunks.clear();
        self.chunker.clear();

        let object_size = self.get_handle().size;

        let new_position = match pos {
            SeekFrom::Start(offset) => min(object_size, offset),
            SeekFrom::End(offset) => {
                if offset > object_size as i64 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Attempted to seek to a negative offset.",
                    ));
                } else {
                    min(object_size, (object_size as i64 - offset) as u64)
                }
            }
            SeekFrom::Current(offset) => {
                if self.position as i64 + offset < 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Attempted to seek to a negative offset.",
                    ));
                } else {
                    min(object_size, (self.position as i64 + offset) as u64)
                }
            }
        };

        self.position = new_position;
        Ok(new_position)
    }
}

// Content-defined chunking makes writing and seeking more complicated. Chunks can't be modified
// in-place; they can only be read or written in their entirety. This means we need to do a lot of
// buffering to wait for a chunk boundary before writing a chunk to the repository. It also means
// the user needs to explicitly call `flush` when they're done writing data.
impl<'a, K: Key, S: DataStore> Write for Object<'a, K, S> {
    /// The `io::Error` returned by this method can be converted into a `data_store::Error`.
    ///
    /// # Errors
    /// - `Error::InvalidData`: Ciphertext verification failed.
    /// - `Error::Store`: An error occurred with the data store.
    /// - `Error::Io`: An I/O error occurred.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Check if this is the first time `write` is being called after calling `flush`.
        if self.chunker.is_empty() {
            // Because we're starting a new write, we need to set the starting location.
            self.start_location = self.current_chunk();

            if let Some(location) = &self.start_location {
                // We need to make sure the data before the seek position is saved when we replace
                // the chunk. Read this data from the repository and write it to the chunker.
                let first_chunk = self.repository.read_chunk(&location.chunk.hash)?;
                self.chunker
                    .write_all(&first_chunk[..location.relative_position()])?;
            }
        }

        // Chunk the data and write any complete chunks to the repository.
        self.chunker.write_all(buf)?;
        self.write_chunks()?;

        // Advance the seek position.
        self.position += buf.len() as u64;

        Ok(buf.len())
    }

    /// The `io::Error` returned by this method can be converted into a `data_store::Error`.
    ///
    /// # Errors
    /// - `Error::InvalidData`: Ciphertext verification failed.
    /// - `Error::Store`: An error occurred with the data store.
    /// - `Error::Io`: An I/O error occurred.
    fn flush(&mut self) -> io::Result<()> {
        let current_chunk = self.current_chunk();

        if let Some(location) = &current_chunk {
            // We need to make sure the data after the seek position is saved when we replace the
            // current chunk. Read this data from the repository and write it to the chunker.
            let last_chunk = self.repository.read_chunk(&location.chunk.hash)?;
            self.chunker
                .write_all(&last_chunk[location.relative_position()..])?;
        }

        // Write all the remaining data in the chunker to the repository.
        self.chunker.flush()?;
        self.write_chunks()?;

        // Find the index of the first chunk which is being overwritten.
        let start_index = self
            .start_location
            .as_ref()
            .map(|location| location.index)
            .unwrap_or(0);

        // Find the index of the last chunk which is being overwritten.
        let end_index = match &current_chunk {
            Some(location) => location.index + 1,
            None => self.get_handle().chunks.len(),
        };

        // Update chunk references in the object handle to reflect changes.
        let new_chunks = replace(&mut self.new_chunks, Vec::new());
        self.get_handle_mut()
            .chunks
            .splice(start_index..end_index, new_chunks);

        // Update the size of the object in the object handle to reflect changes.
        self.get_handle_mut().size = self
            .get_handle_mut()
            .chunks
            .iter()
            .map(|chunk| chunk.size as u64)
            .sum();

        self.start_location = None;

        Ok(())
    }
}

// To avoid reading the same chunk from the repository multiple times, the chunk which was most
// recently read from is cached in a buffer.
impl<'a, K: Key, S: DataStore> Read for Object<'a, K, S> {
    /// The `io::Error` returned by this method can be converted into a `data_store::Error`.
    ///
    /// # Errors
    /// - `Error::InvalidData`: Ciphertext verification failed.
    /// - `Error::Store`: An error occurred with the data store.
    /// - `Error::Io`: An I/O error occurred.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let next_chunk = self.read_chunk(buf.len())?;
        let bytes_read = next_chunk.len();
        buf[..bytes_read].copy_from_slice(next_chunk);
        self.position += bytes_read as u64;
        Ok(bytes_read)
    }
}
