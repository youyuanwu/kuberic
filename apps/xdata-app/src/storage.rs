use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::fs;
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, warn};

pub use hello_world::storage_server::{Storage, StorageServer};
use hello_world::{
    DeleteFileReply, DeleteFileRequest, ListFilesReply, ListFilesRequest, ReadFileReply,
    ReadFileRequest, WriteFileReply, WriteFileRequest,
};
use xedio_shared::proto::hello_world;

const DATA_DIR: &str = "/data";

#[derive(Debug, Clone)]
pub struct MyStorage {
    // Map of filename to lock for coordinating access
    locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl Default for MyStorage {
    fn default() -> Self {
        Self {
            locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl MyStorage {
    async fn get_file_lock(&self, filename: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        locks
            .entry(filename.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

#[tonic::async_trait]
impl Storage for MyStorage {
    async fn write_file(
        &self,
        request: Request<WriteFileRequest>,
    ) -> Result<Response<WriteFileReply>, Status> {
        let req = request.into_inner();

        debug!("Writing to file: {}", req.filename);

        // Validate filename to prevent path traversal
        if req.filename.contains("..") || req.filename.contains('/') {
            warn!("Invalid filename attempted: {}", req.filename);
            return Ok(Response::new(WriteFileReply {
                success: false,
                message: "Invalid filename".to_string(),
            }));
        }

        // Acquire file lock
        let file_lock = self.get_file_lock(&req.filename).await;
        let _guard = file_lock.lock().await;

        let file_path = Path::new(DATA_DIR).join(&req.filename);

        match fs::write(&file_path, req.content).await {
            Ok(_) => {
                info!("Successfully wrote to file: {}", req.filename);
                Ok(Response::new(WriteFileReply {
                    success: true,
                    message: format!("File written successfully: {}", req.filename),
                }))
            }
            Err(e) => {
                error!("Failed to write file {}: {}", req.filename, e);
                Ok(Response::new(WriteFileReply {
                    success: false,
                    message: format!("Failed to write file: {}", e),
                }))
            }
        }
    }

    async fn read_file(
        &self,
        request: Request<ReadFileRequest>,
    ) -> Result<Response<ReadFileReply>, Status> {
        let req = request.into_inner();

        debug!("Reading from file: {}", req.filename);

        // Validate filename to prevent path traversal
        if req.filename.contains("..") || req.filename.contains('/') {
            warn!("Invalid filename attempted: {}", req.filename);
            return Ok(Response::new(ReadFileReply {
                success: false,
                content: String::new(),
                message: "Invalid filename".to_string(),
            }));
        }

        // Acquire file lock
        let file_lock = self.get_file_lock(&req.filename).await;
        let _guard = file_lock.lock().await;

        let file_path = Path::new(DATA_DIR).join(&req.filename);

        match fs::read_to_string(&file_path).await {
            Ok(content) => {
                info!("Successfully read file: {}", req.filename);
                Ok(Response::new(ReadFileReply {
                    success: true,
                    content,
                    message: format!("File read successfully: {}", req.filename),
                }))
            }
            Err(e) => {
                error!("Failed to read file {}: {}", req.filename, e);
                Ok(Response::new(ReadFileReply {
                    success: false,
                    content: String::new(),
                    message: format!("Failed to read file: {}", e),
                }))
            }
        }
    }

    async fn delete_file(
        &self,
        request: Request<DeleteFileRequest>,
    ) -> Result<Response<DeleteFileReply>, Status> {
        let req = request.into_inner();

        debug!("Deleting file: {}", req.filename);

        // Validate filename to prevent path traversal
        if req.filename.contains("..") || req.filename.contains('/') {
            warn!("Invalid filename attempted: {}", req.filename);
            return Ok(Response::new(DeleteFileReply {
                success: false,
                message: "Invalid filename".to_string(),
            }));
        }

        // Acquire file lock
        let file_lock = self.get_file_lock(&req.filename).await;
        let _guard = file_lock.lock().await;

        let file_path = Path::new(DATA_DIR).join(&req.filename);

        match fs::remove_file(&file_path).await {
            Ok(_) => {
                // Release the file guard
                drop(_guard);

                // Try to remove the lock from the map if we're the last holder
                // This is best-effort cleanup to prevent unbounded growth
                let mut locks_map = self.locks.lock().await;
                if let Some(lock) = locks_map.get(&req.filename) {
                    // Only remove if we're the only remaining reference
                    // (strong_count = 2: one in map, one in 'lock' variable)
                    if Arc::strong_count(lock) <= 2 {
                        locks_map.remove(&req.filename);
                        debug!("Removed lock for deleted file: {}", req.filename);
                    }
                }
                drop(locks_map);

                info!("Successfully deleted file: {}", req.filename);
                Ok(Response::new(DeleteFileReply {
                    success: true,
                    message: format!("File deleted successfully: {}", req.filename),
                }))
            }
            Err(e) => {
                error!("Failed to delete file {}: {}", req.filename, e);
                Ok(Response::new(DeleteFileReply {
                    success: false,
                    message: format!("Failed to delete file: {}", e),
                }))
            }
        }
    }

    async fn list_files(
        &self,
        _request: Request<ListFilesRequest>,
    ) -> Result<Response<ListFilesReply>, Status> {
        debug!("Listing files in {}", DATA_DIR);

        let mut filenames = Vec::new();

        match fs::read_dir(DATA_DIR).await {
            Ok(mut entries) => {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    if let Ok(metadata) = entry.metadata().await
                        && metadata.is_file()
                        && let Ok(name) = entry.file_name().into_string()
                    {
                        filenames.push(name);
                    }
                }

                info!("Found {} files", filenames.len());
                Ok(Response::new(ListFilesReply {
                    success: true,
                    filenames,
                    message: "Files listed successfully".to_string(),
                }))
            }
            Err(e) => {
                error!("Failed to list files: {}", e);
                Ok(Response::new(ListFilesReply {
                    success: false,
                    filenames: Vec::new(),
                    message: format!("Failed to list files: {}", e),
                }))
            }
        }
    }
}
