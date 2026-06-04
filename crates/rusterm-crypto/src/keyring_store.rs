use keyring::Entry;

const SERVICE_NAME: &str = "RusTerm";

pub struct KeyringStore;

impl KeyringStore {
    pub fn save_credential(name: &str, secret: &str) -> anyhow::Result<()> {
        let entry = Entry::new(SERVICE_NAME, name)?;
        entry.set_password(secret)?;
        Ok(())
    }

    pub fn get_credential(name: &str) -> anyhow::Result<String> {
        let entry = Entry::new(SERVICE_NAME, name)?;
        let password = entry.get_password()?;
        Ok(password)
    }

    pub fn delete_credential(name: &str) -> anyhow::Result<()> {
        let entry = Entry::new(SERVICE_NAME, name)?;
        entry.delete_credential()?;
        Ok(())
    }
}
