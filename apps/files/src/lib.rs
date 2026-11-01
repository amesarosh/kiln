//! Shared by the file picker (`wayland-time-files`) and the file manager
//! (`wayland-time-file-manager`): folder listings, places, file types and icons, the trash,
//! and the pieces of the window both draw the same way. The image viewer uses the trash too.

pub mod fs;
pub mod trash;
pub mod view;
