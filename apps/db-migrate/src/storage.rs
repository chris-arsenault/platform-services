use async_trait::async_trait;
use std::collections::HashMap;

pub struct MigrationFile {
    pub key: String,
    pub filename: String,
}

#[async_trait]
pub trait FileStore: Send + Sync {
    async fn list_files(
        &self,
        prefix: &str,
    ) -> Result<Vec<MigrationFile>, Box<dyn std::error::Error + Send + Sync>>;
    async fn read_file(
        &self,
        key: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>>;
}

#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn get_param(
        &self,
        name: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>>;
    async fn get_secret(
        &self,
        name: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>>;
    async fn put_param(
        &self,
        name: &str,
        value: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    async fn put_secret(
        &self,
        name: &str,
        value: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// In-memory file store for testing
#[derive(Default)]
pub struct MemoryFileStore {
    files: HashMap<String, String>,
}

impl MemoryFileStore {
    pub fn new() -> Self {
        Self {
            files: HashMap::new(),
        }
    }

    pub fn add_file(&mut self, key: &str, content: &str) {
        self.files.insert(key.to_string(), content.to_string());
    }
}

#[async_trait]
impl FileStore for MemoryFileStore {
    async fn list_files(
        &self,
        prefix: &str,
    ) -> Result<Vec<MigrationFile>, Box<dyn std::error::Error + Send + Sync>> {
        let prefix_depth = prefix.matches('/').count();
        let mut files: Vec<MigrationFile> = self
            .files
            .keys()
            .filter(|k| {
                k.starts_with(prefix)
                    && k.ends_with(".sql")
                    && k.matches('/').count() == prefix_depth
            })
            .map(|k| MigrationFile {
                key: k.clone(),
                filename: k.rsplit('/').next().unwrap_or(k).to_string(),
            })
            .collect();
        files.sort_by(|a, b| a.filename.cmp(&b.filename));
        Ok(files)
    }

    async fn read_file(
        &self,
        key: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        self.files
            .get(key)
            .cloned()
            .ok_or_else(|| format!("File not found: {key}").into())
    }
}

/// In-memory credential store for testing
#[derive(Default)]
pub struct MemoryCredentialStore {
    params: std::sync::Mutex<HashMap<String, String>>,
}

impl MemoryCredentialStore {
    pub fn new() -> Self {
        Self {
            params: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn get_all(&self) -> HashMap<String, String> {
        self.params.lock().unwrap().clone()
    }
}

#[async_trait]
impl CredentialStore for MemoryCredentialStore {
    async fn get_param(
        &self,
        name: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        self.params
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| format!("Param not found: {name}").into())
    }

    async fn get_secret(
        &self,
        name: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        self.get_param(name).await
    }

    async fn put_param(
        &self,
        name: &str,
        value: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.params
            .lock()
            .unwrap()
            .insert(name.to_string(), value.to_string());
        Ok(())
    }

    async fn put_secret(
        &self,
        name: &str,
        value: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.put_param(name, value).await
    }
}
