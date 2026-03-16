use crate::error::Result;

pub trait KeyBackend: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>>;
    fn set(&self, key: &str, value: &str) -> Result<()>;
    fn delete(&self, key: &str) -> Result<()>;
}

pub struct KeyManager {
    backend: Box<dyn KeyBackend>,
}

impl KeyManager {
    pub fn new(backend: Box<dyn KeyBackend>) -> Self {
        Self { backend }
    }

    pub fn with_env() -> Self {
        Self::new(Box::new(EnvVarBackend))
    }

    pub fn get_key(&self, provider: &str, profile: &str) -> Result<Option<String>> {
        let key = format!("{provider}:{profile}");

        if let Some(val) = self.backend.get(&key)? {
            return Ok(Some(val));
        }

        let env_key = format!(
            "{}_API_KEY",
            provider.to_uppercase().replace('-', "_")
        );
        if let Ok(val) = std::env::var(&env_key) {
            return Ok(Some(val));
        }

        Ok(None)
    }

    pub fn set_key(&self, provider: &str, profile: &str, value: &str) -> Result<()> {
        let key = format!("{provider}:{profile}");
        self.backend.set(&key, value)
    }

    pub fn delete_key(&self, provider: &str, profile: &str) -> Result<()> {
        let key = format!("{provider}:{profile}");
        self.backend.delete(&key)
    }

    pub fn has_key(&self, provider: &str, profile: &str) -> bool {
        self.get_key(provider, profile)
            .map(|k| k.is_some())
            .unwrap_or(false)
    }
}

struct EnvVarBackend;

impl KeyBackend for EnvVarBackend {
    fn get(&self, key: &str) -> Result<Option<String>> {
        let env_key = key.replace(':', "_").to_uppercase();
        Ok(std::env::var(&env_key).ok())
    }

    fn set(&self, _key: &str, _value: &str) -> Result<()> {
        Ok(())
    }

    fn delete(&self, _key: &str) -> Result<()> {
        Ok(())
    }
}

pub struct InMemoryBackend {
    data: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl InMemoryBackend {
    pub fn new() -> Self {
        Self {
            data: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

impl KeyBackend for InMemoryBackend {
    fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.data.lock().unwrap().get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        self.data
            .lock()
            .unwrap()
            .insert(key.to_string(), value.to_string());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.data.lock().unwrap().remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manager() -> KeyManager {
        KeyManager::new(Box::new(InMemoryBackend::new()))
    }

    #[test]
    fn set_and_get_key() {
        let mgr = test_manager();
        mgr.set_key("anthropic", "default", "sk-test").unwrap();
        let key = mgr.get_key("anthropic", "default").unwrap();
        assert_eq!(key, Some("sk-test".to_string()));
    }

    #[test]
    fn get_nonexistent_key() {
        let mgr = test_manager();
        let key = mgr.get_key("anthropic", "default").unwrap();
        assert!(key.is_none());
    }

    #[test]
    fn delete_key() {
        let mgr = test_manager();
        mgr.set_key("anthropic", "default", "sk-test").unwrap();
        mgr.delete_key("anthropic", "default").unwrap();
        assert!(!mgr.has_key("anthropic", "default"));
    }

    #[test]
    fn has_key_check() {
        let mgr = test_manager();
        assert!(!mgr.has_key("anthropic", "default"));
        mgr.set_key("anthropic", "default", "sk-test").unwrap();
        assert!(mgr.has_key("anthropic", "default"));
    }
}
