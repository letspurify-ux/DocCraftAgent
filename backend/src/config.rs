use crate::model::Settings;
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, OsRng, rand_core::RngCore},
};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

pub struct Vault {
    key: [u8; 32],
    pub dir: PathBuf,
}
impl Vault {
    pub fn open(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        private_dir(&dir)?;
        let path = dir.join("master.key");
        let key = if path.exists() {
            std::fs::read(&path)?
                .try_into()
                .map_err(|_| anyhow::anyhow!("Invalid master key"))?
        } else {
            let mut key = [0; 32];
            OsRng.fill_bytes(&mut key);
            atomic_private(&path, &key)?;
            key
        };
        Ok(Self { key, dir })
    }
    pub fn encrypt(&self, text: &str) -> Result<String> {
        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|_| anyhow::anyhow!("Cipher setup failed"))?;
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let bytes = cipher
            .encrypt(Nonce::from_slice(&nonce), text.as_bytes())
            .map_err(|_| anyhow::anyhow!("Encryption failed"))?;
        let mut data = nonce.to_vec();
        data.extend(bytes);
        Ok(STANDARD.encode(data))
    }
    pub fn decrypt(&self, text: &str) -> Result<String> {
        let bytes = STANDARD.decode(text)?;
        if bytes.len() < 12 {
            bail!("Invalid encrypted settings");
        }
        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|_| anyhow::anyhow!("Cipher setup failed"))?;
        let (nonce, data) = bytes.split_at(12);
        String::from_utf8(
            cipher
                .decrypt(Nonce::from_slice(nonce), data)
                .map_err(|_| anyhow::anyhow!("Cannot decrypt settings"))?,
        )
        .context("Invalid settings encoding")
    }
    pub fn load(&self) -> Result<Settings> {
        let path = self.dir.join("settings.enc");
        if path.exists() {
            return Ok(serde_json::from_str(
                &self.decrypt(&std::fs::read_to_string(path)?)?,
            )?);
        }
        let mut settings = Settings::default();
        settings.db.password = std::env::var("DOCCRAFT_DB_PASSWORD").unwrap_or_default();
        if let Ok(name) = std::env::var("DOCCRAFT_DB_NAME") {
            settings.db.database = name;
        }
        self.save(&settings)?;
        Ok(settings)
    }
    pub fn save(&self, settings: &Settings) -> Result<()> {
        atomic_private(
            &self.dir.join("settings.enc"),
            self.encrypt(&serde_json::to_string(settings)?)?.as_bytes(),
        )
    }
}
pub fn atomic_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Path needs parent")?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
fn private_dir(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME").context("Windows user identity unavailable")?;
        let domain = std::env::var("USERDOMAIN").unwrap_or_default();
        let principal = if domain.is_empty() {
            user
        } else {
            format!("{domain}\\{user}")
        };
        let status = std::process::Command::new("icacls")
            .arg(path)
            .args(["/inheritance:r", "/grant:r"])
            .arg(format!("{principal}:(OI)(CI)F"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if !status.success() {
            bail!("Cannot restrict local data directory ACL");
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
pub fn redacted(mut s: Settings) -> Settings {
    for secret in [
        &mut s.db.password,
        &mut s.llm.api_key,
        &mut s.llm.proxy_password,
    ] {
        if !secret.is_empty() {
            *secret = "********".into();
        }
    }
    s
}
pub fn preserve_secrets(new: &mut Settings, old: &Settings) {
    if new.db.password == "********" {
        new.db.password = old.db.password.clone();
    }
    if new.llm.api_key == "********" {
        new.llm.api_key = old.llm.api_key.clone();
    }
    if new.llm.proxy_password == "********" {
        new.llm.proxy_password = old.llm.proxy_password.clone();
    }
}
pub fn validate(s: &Settings) -> Result<()> {
    for root in s.source_roots.iter().chain(s.output_roots.iter()) {
        if !root.trim().is_empty() && (!Path::new(root).is_absolute() || !Path::new(root).is_dir())
        {
            bail!("Allowed roots must be existing absolute directories");
        }
    }
    if s.retention_days == 0 || s.retention_days > 3650 || s.cache_max_mb > 102400 {
        bail!("Invalid retention/cache limits");
    }
    if s.db.database.is_empty()
        || !s
            .db
            .database
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_')
    {
        bail!("Database name must contain only letters, numbers, underscores");
    }
    if s.db.database != "doccraft_agent" && s.db.database != "doccraft_agent_test" {
        bail!("Use dedicated doccraft_agent or doccraft_agent_test database");
    }
    if !(1..=32).contains(&s.db.max_connections)
        || !(1..=8).contains(&s.max_jobs)
        || !(1..=16).contains(&s.llm.concurrency)
    {
        bail!("Invalid concurrency limits");
    }
    if s.max_file_bytes == 0
        || s.max_file_bytes > 100 * 1024 * 1024
        || s.max_files == 0
        || s.max_files > 1_000_000
    {
        bail!("Invalid source size limits");
    }
    let l = &s.llm;
    if l.context_limit < 1024
        || l.context_limit > 200_000
        || l.model_context_limit < 1024
        || l.max_output_tokens == 0
        || l.max_output_tokens > l.model_max_output
        || l.safety_percent < 5
        || l.safety_percent > 80
    {
        bail!("Invalid context/output limits (application maximum: 200,000)");
    }
    if l.max_output_tokens as u64 + 512
        >= (l.context_limit.min(l.model_context_limit) as u64 * (100 - l.safety_percent) as u64
            / 100)
    {
        bail!("No room left for input after reserving output and safety margin");
    }
    if !["estimate", "server"].contains(&l.token_mode.as_str())
        || (l.token_mode == "server" && l.token_count_url.is_empty())
    {
        bail!("Token mode must be estimate or server with a counting URL");
    }
    if !["max_tokens", "max_completion_tokens"].contains(&l.output_parameter.as_str()) {
        bail!("Unsupported output parameter");
    }
    if !["default", "off", "on"].contains(&l.reasoning.as_str())
        || !["reasoning_effort", "enable_thinking"].contains(&l.reasoning_parameter.as_str())
    {
        bail!("Unsupported reasoning mapping");
    }
    if !["minimal", "low", "medium", "high", "xhigh", "max"].contains(&l.effort.as_str()) {
        bail!("Invalid reasoning effort");
    }
    if !["none", "system", "custom"].contains(&l.proxy_mode.as_str())
        || (l.proxy_mode == "custom" && l.proxy_url.is_empty())
    {
        bail!("Invalid proxy configuration");
    }
    if l.timeout_seconds < 5
        || l.timeout_seconds > 900
        || l.retries > 6
        || l.rpm == 0
        || l.tpm < 1024
    {
        bail!("Invalid API timeout/rate/retry limits");
    }
    if !l.input_price.is_finite()
        || !l.output_price.is_finite()
        || l.input_price < 0.0
        || l.output_price < 0.0
    {
        bail!("Invalid token prices");
    }
    let url = reqwest::Url::parse(&l.base_url).context("Invalid API URL")?;
    if !["http", "https"].contains(&url.scheme())
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("API URL must be HTTP(S), without credentials, query or fragment");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encrypted_settings_round_trip_and_tampering() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let vault = Vault::open(dir.path().to_path_buf())?;
        let mut settings = Settings::default();
        settings.db.password = "private-test-value".into();
        vault.save(&settings)?;
        let bytes = std::fs::read(dir.path().join("settings.enc"))?;
        assert!(!String::from_utf8_lossy(&bytes).contains("private-test-value"));
        assert_eq!(vault.load()?.db.password, "private-test-value");
        assert!(vault.decrypt("aW52YWxpZA==").is_err());
        Ok(())
    }
    #[test]
    fn reject_unsafe_settings() {
        let mut s = Settings::default();
        s.llm.context_limit = 200001;
        assert!(validate(&s).is_err());
        s = Settings::default();
        s.db.database = "mysql".into();
        assert!(validate(&s).is_err());
        s = Settings::default();
        s.llm.max_output_tokens = 190000;
        assert!(validate(&s).is_err());
    }
}
