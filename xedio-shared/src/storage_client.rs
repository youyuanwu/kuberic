use crate::proto::hello_world::{
    DeleteFileReply, DeleteFileRequest, ListFilesRequest, ReadFileReply, ReadFileRequest,
    WriteFileReply, WriteFileRequest, storage_client::StorageClient,
};
use tonic::transport::Channel;

/// A wrapper around StorageClient that operates on a single file
pub struct FileHandle {
    client: StorageClient<Channel>,
    filename: String,
}

impl FileHandle {
    /// Create a new FileHandle for a specific file
    pub fn new(client: StorageClient<Channel>, filename: String) -> Self {
        Self { client, filename }
    }

    /// Write content to the file
    pub async fn write(&mut self, content: String) -> Result<WriteFileReply, tonic::Status> {
        let request = tonic::Request::new(WriteFileRequest {
            filename: self.filename.clone(),
            content,
        });
        self.client
            .write_file(request)
            .await
            .map(|r| r.into_inner())
    }

    /// Read content from the file
    pub async fn read(&mut self) -> Result<ReadFileReply, tonic::Status> {
        let request = tonic::Request::new(ReadFileRequest {
            filename: self.filename.clone(),
        });
        self.client.read_file(request).await.map(|r| r.into_inner())
    }

    /// Delete the file
    pub async fn delete(&mut self) -> Result<DeleteFileReply, tonic::Status> {
        let request = tonic::Request::new(DeleteFileRequest {
            filename: self.filename.clone(),
        });
        self.client
            .delete_file(request)
            .await
            .map(|r| r.into_inner())
    }

    /// Check if the file exists
    /// Returns true if the file exists, false if it doesn't exist or on error
    pub async fn exists(&mut self) -> bool {
        let request = tonic::Request::new(ListFilesRequest {});
        match self.client.list_files(request).await {
            Ok(response) => {
                let reply = response.into_inner();
                reply.success && reply.filenames.contains(&self.filename)
            }
            Err(_) => false,
        }
    }

    /// Get the filename
    pub fn filename(&self) -> &str {
        &self.filename
    }

    /// Get a mutable reference to the underlying client
    pub fn client_mut(&mut self) -> &mut StorageClient<Channel> {
        &mut self.client
    }

    /// Consume self and return the underlying client
    pub fn into_client(self) -> StorageClient<Channel> {
        self.client
    }
}

/// Extension trait for StorageClient to easily create FileHandles
pub trait StorageClientExt {
    /// Create a FileHandle for operating on a specific file
    fn file(self, filename: String) -> FileHandle;
}

impl StorageClientExt for StorageClient<Channel> {
    fn file(self, filename: String) -> FileHandle {
        FileHandle::new(self, filename)
    }
}
