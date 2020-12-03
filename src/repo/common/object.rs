/*
 * Copyright 2019-2020 Wren Powell
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

use std::clone::Clone;
use std::cmp::min;
use std::fmt::Debug;
use std::hash::Hash;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::mem::replace;

use rmp_serde::{from_read, to_vec};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::chunk_store::{ReadChunk, StoreReader, StoreWriter, WriteChunk};
use super::id_table::UniqueId;
use super::state::RepoState;
use super::state::{ChunkLocation, ObjectState};

/// A checksum used for uniquely identifying a chunk.
pub type ChunkHash = [u8; blake3::OUT_LEN];

/// Compute the BLAKE2 checksum of the given `data` and return the result.
pub fn chunk_hash(data: &[u8]) -> ChunkHash {
    blake3::hash(data).into()
}

/// A chunk of data generated by the chunking algorithm.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Chunk {
    /// The size of the chunk in bytes.
    pub size: usize,

    /// The checksum of the chunk.
    pub hash: ChunkHash,
}

/// A handle for accessing data in a repository.
///
/// An `ObjectHandle` is like an address for locating data stored in an `ObjectRepo`. It can't
/// be used to read or write data directly, but it can be used with `ObjectRepo`to get an
/// `Object` or a `ReadOnlyObject`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ObjectHandle {
    // We need both the `repo_id` and the `instance_id` to uniquely identify the instance an object
    // handle is associated with because a user could give two instances from different repositories
    // the same instance ID.
    /// The UUID of the repository this handle is associated with.
    pub(super) repo_id: Uuid,

    /// The UUID of the repository instance this handle is associated with.
    pub(super) instance_id: Uuid,

    // We could just give each handle a unique UUID instead of having the repository ID, instance
    // ID, *and* handle ID, but using `UniqueId` instead of `Uuid` uses less memory in the
    // `ObjectRepo`. If we used UUIDs, the repository would have to store a UUID in memory for
    // every object, whereas `IdTable` is much more memory efficient.
    /// The ID of this handle which is unique within its repository.
    ///
    /// Handle IDs are only guaranteed to be unique within the same repository.
    pub(super) handle_id: UniqueId,

    /// The original size of the data in bytes.
    pub(super) size: u64,

    /// The checksums of the chunks which make up the data.
    pub(super) chunks: Vec<Chunk>,
}

impl ObjectHandle {
    /// Return a `ContentId` representing the contents of the object.
    ///
    /// Calculating a content ID is cheap. This method does not read any data from the data store.
    ///
    /// The returned `ContentId` represents the contents of the object at the time this method was
    /// called. It is not updated when the object is modified.
    pub fn content_id(&self) -> ContentId {
        ContentId {
            repo_id: self.repo_id,
            size: self.size,
            chunks: self.chunks.clone(),
        }
    }

    /// The size of the object in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Return whether this object has the same contents as `other`.
    ///
    /// See `ContentId::compare_contents` for details.
    pub fn compare_contents(&self, other: impl Read) -> crate::Result<bool> {
        self.content_id().compare_contents(other)
    }
}

/// A value that uniquely identifies the contents of an object at a certain point in time.
///
/// A `ContentId` is like a checksum of the data in an object except it is cheap to compute.
/// A `ContentId` can be compared with other `ContentId` values to determine if the contents of two
/// objects are equal. However, these comparisons are only valid within the same repository;
/// content IDs from different repositories are never equal. To compare data between repositories,
/// you should use `compare_contents`.
///
/// `ContentId` is opaque, but it can be serialized and deserialized. The value of a `ContentId` is
/// stable, meaning that they can be compared across invocations of the library.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub struct ContentId {
    // We can't compare content IDs from different repositories because those repositories may have
    // different a chunking configuration. To ensure consistent behavior, we include the
    // repository's UUID to ensure that content IDs from different repositories are never equal.
    /// The ID of the repository the object is associated with.
    pub(super) repo_id: Uuid,

    /// The original size of the data in bytes.
    pub(super) size: u64,

    /// The checksums of the chunks which make up the data.
    pub(super) chunks: Vec<Chunk>,
}

impl ContentId {
    /// The size of the contents represented by this content ID in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Return whether this content ID has the same contents as `other`.
    ///
    /// This compares the contents of this content ID with `other` without reading any data from the
    /// data store. This is much faster than calculating a checksum of the object, especially if
    /// reading from the data store would be prohibitively slow.
    ///
    /// This method compares this content ID with `other` in chunks, and will fail early if any
    /// chunk does not match. This means that it may not be necessary to read `other` in its
    /// entirety to determine that the contents are different.
    ///
    /// Because `other` only implements `Read`, this cannot compare the contents by size. If you
    /// need to compare this content ID with a file or some other source of data with a known size,
    /// you should use `size` to query the size of this content ID so you can handle the trivial
    /// case of the contents having different sizes.
    ///
    /// If you need to compare the contents of two objects from the same repository, it's cheaper to
    /// check if their `ContentId` values are equal instead.
    ///
    /// # Errors
    /// - `Error::Io`: An I/O error occurred.
    pub fn compare_contents(&self, mut other: impl Read) -> crate::Result<bool> {
        let mut buffer = Vec::new();

        for chunk in &self.chunks {
            // Grow the buffer so it's large enough.
            if buffer.len() < chunk.size {
                buffer.resize(chunk.size, 0u8);
            }

            if let Err(error) = other.read_exact(&mut buffer[..chunk.size]) {
                return if error.kind() == io::ErrorKind::UnexpectedEof {
                    Ok(false)
                } else {
                    Err(error.into())
                };
            }

            if chunk.hash != chunk_hash(&buffer[..chunk.size]) {
                return Ok(false);
            }
        }

        // Handle the case where `other` is longer than this object.
        if other.read(&mut buffer)? != 0 {
            return Ok(false);
        }

        Ok(true)
    }
}

struct ObjectReader<'a> {
    chunk_reader: &'a mut Box<dyn ReadChunk>,
    object_state: &'a mut ObjectState,
    handle: &'a ObjectHandle,
}

/// A wrapper for reading data from an object.
impl<'a> ObjectReader<'a> {
    /// Verify the integrity of the data in this object.
    fn verify(&mut self) -> crate::Result<bool> {
        let expected_chunks = self.handle.chunks.iter().copied().collect::<Vec<_>>();

        for chunk in expected_chunks {
            match self.chunk_reader.read_chunk(chunk) {
                Ok(data) => {
                    if data.len() != chunk.size || chunk_hash(&data) != chunk.hash {
                        return Ok(false);
                    }
                }
                // Ciphertext verification failed. No need to check the hash.
                Err(crate::Error::InvalidData) => return Ok(false),
                Err(error) => return Err(error),
            }
        }

        Ok(true)
    }

    /// Return the chunk at the current seek position or `None` if there is none.
    fn current_chunk(&self) -> Option<ChunkLocation> {
        let mut chunk_start = 0u64;
        let mut chunk_end = 0u64;

        for (index, chunk) in self.handle.chunks.iter().enumerate() {
            chunk_end += chunk.size as u64;
            if self.object_state.position >= chunk_start && self.object_state.position < chunk_end {
                return Some(ChunkLocation {
                    chunk: *chunk,
                    start: chunk_start,
                    end: chunk_end,
                    position: self.object_state.position,
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
        let current_location = match self.object_reader().current_chunk() {
            Some(location) => location,
            None => return Ok(&[]),
        };

        // If we're reading from a new chunk, read the contents of that chunk into the read buffer.
        if Some(current_location.chunk) != self.object_state.buffered_chunk {
            self.object_state.buffered_chunk = Some(current_location.chunk);
            self.object_state.read_buffer = self.chunk_reader.read_chunk(current_location.chunk)?;
        }

        let start = current_location.relative_position();
        let end = min(start + size, current_location.chunk.size as usize);
        Ok(&self.object_state.read_buffer[start..end])
    }

    /// Attempt to deserialize the bytes in the object as a value of type `T`.
    fn deserialize<T: DeserializeOwned>(&mut self) -> crate::Result<T> {
        self.seek(SeekFrom::Start(0))?;
        from_read(self).map_err(|_| crate::Error::Deserialize)
    }
}

impl<'a> Seek for ObjectReader<'a> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let object_size = self.handle.size;

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
                if self.object_state.position as i64 + offset < 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Attempted to seek to a negative offset.",
                    ));
                } else {
                    min(
                        object_size,
                        (self.object_state.position as i64 + offset) as u64,
                    )
                }
            }
        };

        self.object_state.position = new_position;
        Ok(new_position)
    }
}

// To avoid reading the same chunk from the repository multiple times, the chunk which was most
// recently read from is cached in a buffer.
impl<'a> Read for ObjectReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let next_chunk = self.read_chunk(buf.len())?;
        let bytes_read = next_chunk.len();
        buf[..bytes_read].copy_from_slice(next_chunk);
        self.object_state.position += bytes_read as u64;
        Ok(bytes_read)
    }
}

/// A wrapper for writing data to an object.
struct ObjectWriter<'a> {
    chunk_writer: &'a mut Box<dyn WriteChunk>,
    object_state: &'a mut ObjectState,
    handle: &'a mut ObjectHandle,
}

impl<'a> ObjectWriter<'a> {
    fn object_reader(&mut self) -> ObjectReader {
        ObjectReader {
            chunk_reader: self.chunk_writer,
            object_state: self.object_state,
            handle: self.handle,
        }
    }

    fn truncate(&mut self, length: u64) -> crate::Result<()> {
        self.flush()?;
        if length >= self.handle.size {
            return Ok(());
        }

        let original_position = self.object_state.position;
        self.object_state.position = length;

        // Truncating the object may mean slicing a chunk in half. Because we can't edit chunks
        // in-place, we need to read the final chunk, slice it, and write it back.
        let end_location = match self.object_reader().current_chunk() {
            Some(location) => location,
            None => return Ok(()),
        };
        let last_chunk = self.chunk_writer.read_chunk(end_location.chunk)?;
        let new_last_chunk = &last_chunk[..end_location.relative_position()];
        let new_last_chunk = self
            .chunk_writer
            .write_chunk(&new_last_chunk, self.handle.handle_id)?;

        // Remove all chunks including and after the final chunk.
        self.handle.chunks.drain(end_location.index..);

        // Append the new final chunk which has been sliced.
        self.handle.chunks.push(new_last_chunk);

        // Update the object size.
        let current_size = self.handle.size;
        self.handle.size = min(length, current_size);

        // Restore the seek position.
        self.object_state.position = min(original_position, length);

        Ok(())
    }

    /// Serialize the given `value` and write it to the object.
    fn serialize<T: Serialize>(&mut self, value: &T) -> crate::Result<()> {
        let serialized = to_vec(value).map_err(|_| crate::Error::Serialize)?;
        self.object_reader().seek(SeekFrom::Start(0))?;
        self.write_all(serialized.as_slice())?;
        self.flush()?;
        self.truncate(serialized.len() as u64)?;
        Ok(())
    }

    /// Write chunks stored in the chunker to the repository.
    fn write_chunks(&mut self) -> crate::Result<()> {
        for chunk_data in self.object_state.chunker.chunks() {
            let chunk = self
                .chunk_writer
                .write_chunk(&chunk_data, self.handle.handle_id)?;
            self.object_state.new_chunks.push(chunk);
        }
        Ok(())
    }
}

// Content-defined chunking makes writing and seeking more complicated. Chunks can't be modified
// in-place; they can only be read or written in their entirety. This means we need to do a lot of
// buffering to wait for a chunk boundary before writing a chunk to the repository. It also means
// the user needs to explicitly call `flush` when they're done writing data.
impl<'a> Write for ObjectWriter<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Check if this is the first time `write` is being called after calling `flush`.
        if !self.object_state.needs_flushed {
            // Because we're starting a new write, we need to set the starting location.
            self.object_state.start_location = self.object_reader().current_chunk();

            if let Some(location) = &self.object_state.start_location {
                let chunk = location.chunk;
                let position = location.relative_position();

                // We need to make sure the data before the seek position is saved when we replace
                // the chunk. Read this data from the repository and write it to the chunker.
                let first_chunk = self.chunk_writer.read_chunk(chunk)?;
                self.object_state
                    .chunker
                    .write_all(&first_chunk[..position])?;
            }
        }

        // Chunk the data and write any complete chunks to the repository.
        self.object_state.chunker.write_all(buf)?;
        self.write_chunks()?;

        // Advance the seek position.
        self.object_state.position += buf.len() as u64;

        // Mark that data has been written to the object since it was last flushed.
        self.object_state.needs_flushed = true;

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.object_state.needs_flushed {
            // No new data has been written since data was last flushed.
            return Ok(());
        }

        let current_chunk = self.object_reader().current_chunk();

        if let Some(location) = &current_chunk {
            // We need to make sure the data after the seek position is saved when we replace the
            // current chunk. Read this data from the repository and write it to the chunker.
            let last_chunk = self.chunk_writer.read_chunk(location.chunk)?;
            self.object_state
                .chunker
                .write_all(&last_chunk[location.relative_position()..])?;
        }

        // Write all the remaining data in the chunker to the repository.
        self.object_state.chunker.flush()?;
        self.write_chunks()?;

        // Find the index of the first chunk which is being overwritten.
        let start_index = self
            .object_state
            .start_location
            .as_ref()
            .map(|location| location.index)
            .unwrap_or(0);

        let end_index = {
            // Find the index of the last chunk which is being overwritten.
            match &current_chunk {
                Some(location) => location.index + 1,
                None => self.handle.chunks.len(),
            }
        };

        let new_chunks = replace(&mut self.object_state.new_chunks, Vec::new());

        {
            // Update chunk references in the object handle to reflect changes.
            self.handle
                .chunks
                .splice(start_index..end_index, new_chunks);

            // Update the size of the object in the object handle to reflect changes.
            self.handle.size = self
                .handle
                .chunks
                .iter()
                .map(|chunk| chunk.size as u64)
                .sum();
        }

        self.object_state.start_location = None;
        self.object_state.needs_flushed = false;

        Ok(())
    }
}

/// An read-only view of data in a repository.
///
/// A `ReadOnlyObject` is a view of data in a repository. It implements `Read` and `Seek` for
/// reading data from the repository. You can think of this as a read-only counterpart to `Object`.
///
/// See `Object` for details.
#[derive(Debug)]
pub struct ReadOnlyObject<'a> {
    /// The state for the object repository.
    chunk_reader: Box<dyn ReadChunk>,

    /// The state for the object itself.
    object_state: ObjectState,

    /// The object handle which stores the hashes of the chunks which make up the object.
    handle: &'a ObjectHandle,
}

impl<'a> ReadOnlyObject<'a> {
    pub(crate) fn new(repo_state: &'a RepoState, handle: &'a ObjectHandle) -> Self {
        Self {
            object_state: ObjectState::new(repo_state.metadata.chunking.to_chunker()),
            chunk_reader: Box::new(StoreReader::new(repo_state)),
            handle,
        }
    }

    fn object_reader(&mut self) -> ObjectReader {
        ObjectReader {
            chunk_reader: &mut self.chunk_reader,
            object_state: &mut self.object_state,
            handle: self.handle,
        }
    }

    /// Return the size of the object in bytes.
    ///
    /// Unflushed data is not accounted for when calculating the size, so you may want to explicitly
    /// flush written data with `flush` before calling this method.
    pub fn size(&self) -> u64 {
        self.handle.size()
    }

    /// Return a `ContentId` representing the contents of this object.
    ///
    /// Unflushed data is not accounted for when generating a content ID, so you may want to
    /// explicitly flush written data with `flush` before calling this method.
    ///
    /// See `ObjectHandle::content_id` for details.
    pub fn content_id(&self) -> ContentId {
        self.handle.content_id()
    }

    /// Return whether this object has the same contents as `other`.
    ///
    /// Unflushed data is not accounted for when comparing contents, so you may want to explicitly
    /// flush written data with `flush` before calling this method.
    ///
    /// See `ContentId::compare_contents` for details.
    pub fn compare_contents(&self, other: impl Read) -> crate::Result<bool> {
        self.handle.compare_contents(other)
    }

    /// Verify the integrity of the data in this object.
    ///
    /// See `Object::verify` for details.
    pub fn verify(&mut self) -> crate::Result<bool> {
        self.object_reader().verify()
    }

    /// Deserialize a value serialized with `Object::serialize`.
    ///
    /// See `Object::deserialize` for details.
    pub fn deserialize<T: DeserializeOwned>(&mut self) -> crate::Result<T> {
        self.object_reader().deserialize()
    }
}

impl<'a> Read for ReadOnlyObject<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.object_reader().read(buf)
    }
}

impl<'a> Seek for ReadOnlyObject<'a> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.object_reader().seek(pos)
    }
}

/// A read-write view of data in a repository.
///
/// An `Object` is a view of data in a repository. It implements `Read`, `Write`, and `Seek` for
/// reading data from the repository and writing data to the repository.
///
/// Because `Object` internally buffers data when reading, there's no need to use a buffered reader
/// like `BufReader`.
///
/// Written data is automatically flushed when this value is dropped. If an error occurs while
/// flushing data in the `Drop` implementation, it is ignored and unflushed data is discarded. If
/// you need to handle these errors, you should call `flush` manually.
///
/// If encryption is enabled for the repository, data integrity is automatically verified as it is
/// read and methods will return an `Err` if corrupt data is found. The `verify` method can be used
/// to check the integrity of all the data in the object whether encryption is enabled or not.
///
/// The methods of `Read`, `Write`, and `Seek` return `io::Result`, but the returned `io::Error` can
/// be converted `Into` an `acid_store::Error` to be consistent with the rest of the library. The
/// implementations document which `acid_store::Error` values they can be converted into.
#[derive(Debug)]
pub struct Object<'a> {
    /// The state for the object repository.
    chunk_writer: Box<dyn WriteChunk>,

    /// The state for the object itself.
    object_state: ObjectState,

    /// The object handle which stores the hashes of the chunks which make up the object.
    handle: &'a mut ObjectHandle,
}

impl<'a> Object<'a> {
    pub(crate) fn new(repo_state: &'a mut RepoState, handle: &'a mut ObjectHandle) -> Self {
        let chunker = repo_state.metadata.chunking.to_chunker();
        Self {
            object_state: ObjectState::new(chunker),
            chunk_writer: Box::new(StoreWriter::new(repo_state)),
            handle,
        }
    }

    fn object_reader(&mut self) -> ObjectReader {
        ObjectReader {
            chunk_reader: &mut self.chunk_writer,
            object_state: &mut self.object_state,
            handle: self.handle,
        }
    }

    fn object_writer(&mut self) -> ObjectWriter {
        ObjectWriter {
            chunk_writer: &mut self.chunk_writer,
            object_state: &mut self.object_state,
            handle: self.handle,
        }
    }

    /// Return the size of the object in bytes.
    ///
    /// Unflushed data is not accounted for when calculating the size, so you may want to explicitly
    /// flush written data with `flush` before calling this method.
    pub fn size(&self) -> u64 {
        self.handle.size()
    }

    /// Return a `ContentId` representing the contents of this object.
    ///
    /// Unflushed data is not accounted for when generating a content ID, so you may want to
    /// explicitly flush written data with `flush` before calling this method.
    ///
    /// See `ObjectHandle::content_id` for details.
    pub fn content_id(&self) -> ContentId {
        self.handle.content_id()
    }

    /// Return whether this object has the same contents as `other`.
    ///
    /// Unflushed data is not accounted for when comparing contents, so you may want to explicitly
    /// flush written data with `flush` before calling this method.
    ///
    /// See `ContentId::compare_contents` for details.
    pub fn compare_contents(&self, other: impl Read) -> crate::Result<bool> {
        self.handle.compare_contents(other)
    }

    /// Verify the integrity of the data in this object.
    ///
    /// This returns `true` if the object is valid and `false` if it is corrupt.
    ///
    /// Unflushed data is not accounted for when verifying data integrity, so you may want to
    /// explicitly flush written data with `flush` before calling this method.
    ///
    /// # Errors
    /// - `Error::InvalidData`: Ciphertext verification failed.
    /// - `Error::Store`: An error occurred with the data store.
    /// - `Error::Io`: An I/O error occurred.
    pub fn verify(&mut self) -> crate::Result<bool> {
        self.object_reader().verify()
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
        self.object_writer().truncate(length)
    }

    /// Serialize the given `value` and write it to the object.
    ///
    /// This is a convenience function that serializes the `value` using a space-efficient binary
    /// format, overwrites all the data in the object, and truncates it to the length of the
    /// serialized `value`.
    ///
    /// # Errors
    /// - `Error::Serialize`: The given value could not be serialized.
    /// - `Error::InvalidData`: Ciphertext verification failed.
    /// - `Error::Store`: An error occurred with the data store.
    /// - `Error::Io`: An I/O error occurred.
    pub fn serialize<T: Serialize>(&mut self, value: &T) -> crate::Result<()> {
        self.object_writer().serialize(value)
    }

    /// Deserialize a value serialized with `Object::serialize`.
    ///
    /// This is a convenience function that deserializes a value serialized to the object with
    /// `Object::serialize`
    ///
    /// # Errors
    /// - `Error::Deserialize`: The data could not be deserialized as a value of type `T`.
    /// - `Error::InvalidData`: Ciphertext verification failed.
    /// - `Error::Store`: An error occurred with the data store.
    /// - `Error::Io`: An I/O error occurred.
    pub fn deserialize<T: DeserializeOwned>(&mut self) -> crate::Result<T> {
        self.object_reader().deserialize()
    }
}

impl<'a> Read for Object<'a> {
    /// The `io::Error` returned by this method can be converted into an `acid_store::Error`.
    ///
    /// # Errors
    /// - `Error::InvalidData`: Ciphertext verification failed.
    /// - `Error::Store`: An error occurred with the data store.
    /// - `Error::Io`: An I/O error occurred.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.object_writer().flush()?;
        self.object_reader().read(buf)
    }
}

impl<'a> Seek for Object<'a> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.object_writer().flush()?;
        self.object_reader().seek(pos)
    }
}

impl<'a> Write for Object<'a> {
    /// The `io::Error` returned by this method can be converted into an `acid_store::Error`.
    ///
    /// # Errors
    /// - `Error::InvalidData`: Ciphertext verification failed.
    /// - `Error::Store`: An error occurred with the data store.
    /// - `Error::Io`: An I/O error occurred.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.object_writer().write(buf)
    }

    /// The `io::Error` returned by this method can be converted into an `acid_store::Error`.
    ///
    /// # Errors
    /// - `Error::InvalidData`: Ciphertext verification failed.
    /// - `Error::Store`: An error occurred with the data store.
    /// - `Error::Io`: An I/O error occurred.
    fn flush(&mut self) -> io::Result<()> {
        self.object_writer().flush()
    }
}

impl<'a> Drop for Object<'a> {
    fn drop(&mut self) {
        self.flush().ok();
    }
}
