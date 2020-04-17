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

use std::io::Write;
use std::path::Path;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use rand::rngs::SmallRng;
use rand::{RngCore, SeedableRng};
use tempfile::tempdir;

use acid_store::repo::{ObjectRepository, RepositoryConfig};
use acid_store::store::{DirectoryStore, OpenOption, OpenStore};

/// Return a buffer containing `size` random bytes for testing purposes.
pub fn random_bytes(size: usize) -> Vec<u8> {
    let mut rng = SmallRng::from_entropy();
    let mut buffer = vec![0u8; size];
    rng.fill_bytes(&mut buffer);
    buffer
}

/// Return a new repository in the given `directory` for benchmarking.
pub fn new_repo(directory: &Path) -> ObjectRepository<String, DirectoryStore> {
    ObjectRepository::create_repo(
        DirectoryStore::open(
            directory.join("store"),
            OpenOption::CREATE | OpenOption::TRUNCATE,
        )
        .unwrap(),
        RepositoryConfig::default(),
        None,
    )
    .unwrap()
}

/// The number of bytes to write when a trivial amount of data must be written.
const TRIVIAL_DATA_SIZE: usize = 16;

pub fn insert_object(criterion: &mut Criterion) {
    let tmp_dir = tempdir().unwrap();
    let mut group = criterion.benchmark_group("Insert an object");

    for num_objects in [100, 1_000, 10_000].iter() {
        // Create a new repository.
        let mut repo = new_repo(tmp_dir.path());

        // Insert objects and write to them but don't commit them.
        for i in 0..*num_objects {
            let mut object = repo.insert(String::from(format!("Uncommitted object {}", i)));
            object
                .write_all(random_bytes(TRIVIAL_DATA_SIZE).as_slice())
                .unwrap();
            object.flush().unwrap();
        }

        group.throughput(Throughput::Elements(1));

        // Benchmark inserting a new object.
        group.bench_function(
            format!("with {} uncommitted objects", num_objects),
            |bencher| {
                bencher.iter(|| {
                    repo.insert(String::from("Test"));
                });
            },
        );
    }
}

pub fn insert_object_and_write(criterion: &mut Criterion) {
    let tmp_dir = tempdir().unwrap();
    let mut group = criterion.benchmark_group("Insert an object and write to it");

    for num_objects in [100, 1_000, 10_000].iter() {
        // Create a new repository.
        let mut repo = new_repo(tmp_dir.path());

        // Insert objects and write to them but don't commit them.
        for i in 0..*num_objects {
            let mut object = repo.insert(String::from(format!("Uncommitted object {}", i)));
            object
                .write_all(random_bytes(TRIVIAL_DATA_SIZE).as_slice())
                .unwrap();
            object.flush().unwrap();
        }

        group.throughput(Throughput::Elements(1));

        // Benchmark inserting a new object and writing to it.
        group.bench_function(
            format!("with {} uncommitted objects", num_objects),
            |bencher| {
                bencher.iter_batched(
                    || random_bytes(TRIVIAL_DATA_SIZE),
                    |data| {
                        let mut object = repo.insert(String::from("Test"));
                        object.write_all(data.as_slice()).unwrap();
                        object.flush().unwrap();
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
}

criterion_group!(insert, insert_object, insert_object_and_write);
criterion_main!(insert);
