/*
 * Copyright 2019 Wren Powell
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

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{set_permissions, Metadata, Permissions};
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{symlink, PermissionsExt};
#[cfg(windows)]
use std::os::windows::fs::symlink_file;
use std::path::Path;

/// Get the file mode from the given file `metadata`.
///
/// This will return `None` if POSIX permissions are not supported by the given platform.
pub fn file_mode(metadata: &Metadata) -> Option<u32> {
    #[cfg(unix)]
    return Some(metadata.permissions().mode());

    None
}

/// Set the given file `mode` on the given `file`.
///
/// This does nothing if POSIX permissions are not supported by the current platform.
pub fn set_file_mode(file: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    set_permissions(file, Permissions::from_mode(mode))?;

    Ok(())
}
/// Returns a map of the extended attributes of the given `file`.
///
/// The map will be empty if extended attributes are not supported by the current platform.
pub fn extended_attrs(file: &Path) -> io::Result<HashMap<OsString, Vec<u8>>> {
    let mut attributes = HashMap::new();

    if xattr::SUPPORTED_PLATFORM {
        for attr_name in xattr::list(file)? {
            if let Some(attr_value) = xattr::get(file, &attr_name)? {
                attributes.insert(attr_name, attr_value);
            }
        }
    }

    Ok(attributes)
}

/// Sets the given `attributes` on the given `file`.
///
/// This does nothing if extended attributes are not supported by the current platform.
pub fn set_extended_attrs(file: &Path, attributes: HashMap<OsString, Vec<u8>>) -> io::Result<()> {
    if xattr::SUPPORTED_PLATFORM {
        for (attr_name, attr_value) in attributes.iter() {
            xattr::set(file, attr_name, attr_value)?;
        }
    }

    Ok(())
}

/// Create a symbolic `link` to a given `target` file.
///
/// On Windows, this attempts to create a file link (not a directory link).
pub fn soft_link(link: &Path, target: &Path) -> io::Result<()> {
    #[cfg(windows)]
    symlink_file(target, link)?;

    #[cfg(unix)]
    symlink(target, link)?;

    Ok(())
}
