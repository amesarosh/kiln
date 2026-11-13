//! Shared by the file picker (`kiln-files`) and the file manager
//! (`kiln-file-manager`): folder listings, places, file types and icons, the trash,
//! and the pieces of the window both draw the same way. The image viewer uses the trash too.

pub mod fs;
pub mod trash;
pub mod view;
