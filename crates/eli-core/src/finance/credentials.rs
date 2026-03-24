use serde::Deserialize;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub(crate) struct KalshiWsCredentials {
    pub(crate) key_id: String,
    pub(crate) private_key_pem: String,
}

#[derive(Clone, Debug)]
pub(crate) struct PolymarketCredentials {
    pub(crate) api_key: String,
    pub(crate) secret: String,
    pub(crate) passphrase: String,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct InventoryFile {
    #[serde(default)]
    fred: Option<FredInventory>,
    #[serde(default)]
    kalshi: Option<KalshiInventory>,
    #[serde(default)]
    polymarket: Option<PolymarketInventory>,
    #[serde(default)]
    ibkr: Option<IbkrInventory>,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct FredInventory {
    #[serde(default)]
    api_key: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct KalshiInventory {
    #[serde(default)]
    api_key_id: Option<String>,
    #[serde(default)]
    access_key_id: Option<String>,
    #[serde(default)]
    private_key_pem: Option<String>,
    #[serde(default)]
    private_key_path: Option<String>,
    #[serde(default)]
    pem_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct PolymarketInventory {
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    passphrase: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct IbkrInventory {
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    client_id: Option<i32>,
    #[serde(default)]
    market_data_type: Option<i32>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

pub(crate) fn resolve_kalshi_ws_credentials() -> std::result::Result<KalshiWsCredentials, String> {
    let key_id_env = std::env::var("KALSHI_API_KEY_ID")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("KALSHI_ACCESS_KEY_ID")
                .ok()
                .filter(|v| !v.trim().is_empty())
        });
    let pem_env = std::env::var("KALSHI_API_PRIVATE_KEY_PEM")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("KALSHI_PEM_PATH")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(PathBuf::from)
                .map(read_secret_file)
                .transpose()
                .ok()
                .flatten()
        });

    if let (Some(key_id), Some(private_key_pem)) = (key_id_env.clone(), pem_env.clone()) {
        return Ok(KalshiWsCredentials {
            key_id,
            private_key_pem,
        });
    }

    let inv = read_inventory_file()?;
    let kalshi = inv.and_then(|v| v.kalshi);

    let key_id = key_id_env
        .or_else(|| {
            kalshi
                .as_ref()
                .and_then(|k| normalized(k.api_key_id.as_deref()))
        })
        .or_else(|| {
            kalshi
                .as_ref()
                .and_then(|k| normalized(k.access_key_id.as_deref()))
        })
        .ok_or_else(|| {
            format!(
                "kalshi credentials missing key id; set KALSHI_API_KEY_ID/KALSHI_ACCESS_KEY_ID or configure {}",
                inventory_path().display()
            )
        })?;

    let private_key_pem = pem_env
        .or_else(|| {
            kalshi
                .as_ref()
                .and_then(|k| normalized(k.private_key_pem.as_deref()))
        })
        .or_else(|| {
            kalshi
                .as_ref()
                .and_then(|k| normalized(k.private_key_path.as_deref()))
                .map(PathBuf::from)
                .map(read_secret_file)
                .transpose()
                .ok()
                .flatten()
        })
        .or_else(|| {
            kalshi
                .as_ref()
                .and_then(|k| normalized(k.pem_path.as_deref()))
                .map(PathBuf::from)
                .map(read_secret_file)
                .transpose()
                .ok()
                .flatten()
        })
        .ok_or_else(|| {
            format!(
                "kalshi credentials missing private key; set KALSHI_API_PRIVATE_KEY_PEM/KALSHI_PEM_PATH or configure {}",
                inventory_path().display()
            )
        })?;

    Ok(KalshiWsCredentials {
        key_id,
        private_key_pem,
    })
}

pub(crate) fn resolve_polymarket_credentials() -> std::result::Result<PolymarketCredentials, String>
{
    let api_key_env = std::env::var("POLYMARKET_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let secret_env = std::env::var("POLYMARKET_API_SECRET")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let passphrase_env = std::env::var("POLYMARKET_API_PASSPHRASE")
        .ok()
        .filter(|v| !v.trim().is_empty());

    if let (Some(api_key), Some(secret), Some(passphrase)) = (
        api_key_env.clone(),
        secret_env.clone(),
        passphrase_env.clone(),
    ) {
        return Ok(PolymarketCredentials {
            api_key,
            secret,
            passphrase,
        });
    }

    let inv = read_inventory_file()?;
    let poly = inv.and_then(|v| v.polymarket);

    let api_key = api_key_env
        .or_else(|| poly.as_ref().and_then(|p| normalized(p.api_key.as_deref())))
        .ok_or_else(|| {
            format!(
                "polymarket credentials missing api_key; set POLYMARKET_API_KEY or configure {}",
                inventory_path().display()
            )
        })?;
    let secret = secret_env
        .or_else(|| poly.as_ref().and_then(|p| normalized(p.secret.as_deref())))
        .ok_or_else(|| {
            format!(
                "polymarket credentials missing secret; set POLYMARKET_API_SECRET or configure {}",
                inventory_path().display()
            )
        })?;
    let passphrase = passphrase_env
        .or_else(|| poly.as_ref().and_then(|p| normalized(p.passphrase.as_deref())))
        .ok_or_else(|| {
            format!(
                "polymarket credentials missing passphrase; set POLYMARKET_API_PASSPHRASE or configure {}",
                inventory_path().display()
            )
        })?;

    Ok(PolymarketCredentials {
        api_key,
        secret,
        passphrase,
    })
}

pub(crate) fn resolve_fred_api_key() -> std::result::Result<String, String> {
    let api_key_env = std::env::var("FRED_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty());

    if let Some(api_key) = api_key_env.clone() {
        return Ok(api_key);
    }

    let inv = read_inventory_file()?;
    let fred = inv.as_ref().and_then(|v| v.fred.as_ref());

    let api_key = api_key_env
        .or_else(|| fred.and_then(|f| normalized(f.api_key.as_deref())))
        .or_else(|| {
            // Legacy fallback for users who still have the key in config.toml.
            crate::config::Paths::discover()
                .ok()
                .and_then(|paths| crate::config::load_or_default(&paths).ok())
                .and_then(|cfg| cfg.finance.fred_api_key)
                .filter(|value| !value.trim().is_empty())
        })
        .ok_or_else(|| {
            format!(
                "fred api key missing; set FRED_API_KEY or configure [fred].api_key in {}",
                inventory_path().display()
            )
        })?;

    Ok(api_key)
}

pub(crate) fn has_fred_api_configuration_hint() -> bool {
    let env_present = std::env::var("FRED_API_KEY")
        .ok()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    if env_present {
        return true;
    }

    if let Ok(Some(inv)) = read_inventory_file() {
        if inv
            .fred
            .as_ref()
            .and_then(|fred| normalized(fred.api_key.as_deref()))
            .is_some()
        {
            return true;
        }
    }

    crate::config::Paths::discover()
        .ok()
        .and_then(|paths| crate::config::load_or_default(&paths).ok())
        .and_then(|cfg| cfg.finance.fred_api_key)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

pub(crate) fn resolve_ibkr_connection(
    overrides: Option<&eli_finance_types::IbkrConnectionConfig>,
) -> std::result::Result<eli_finance_types::IbkrConnectionConfig, String> {
    let inv = read_inventory_file()?;
    let ibkr = inv.and_then(|v| v.ibkr);

    let config = eli_finance_types::IbkrConnectionConfig {
        account: overrides
            .and_then(|v| normalized(v.account.as_deref()))
            .or_else(|| {
                std::env::var("IBKR_ACCOUNT")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
            })
            .or_else(|| ibkr.as_ref().and_then(|v| normalized(v.account.as_deref()))),
        host: overrides
            .and_then(|v| normalized(v.host.as_deref()))
            .or_else(|| {
                std::env::var("IBKR_HOST")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
            })
            .or_else(|| ibkr.as_ref().and_then(|v| normalized(v.host.as_deref()))),
        port: overrides
            .and_then(|v| v.port)
            .or_else(|| {
                std::env::var("IBKR_PORT")
                    .ok()
                    .and_then(|v| v.parse::<u16>().ok())
            })
            .or_else(|| ibkr.as_ref().and_then(|v| v.port)),
        client_id: overrides
            .and_then(|v| v.client_id)
            .or_else(|| {
                std::env::var("IBKR_CLIENT_ID")
                    .ok()
                    .and_then(|v| v.parse::<i32>().ok())
            })
            .or_else(|| ibkr.as_ref().and_then(|v| v.client_id)),
        market_data_type: overrides
            .and_then(|v| v.market_data_type)
            .or_else(|| {
                std::env::var("IBKR_MARKET_DATA_TYPE")
                    .ok()
                    .and_then(|v| v.parse::<i32>().ok())
            })
            .or_else(|| ibkr.as_ref().and_then(|v| v.market_data_type)),
        timeout_secs: overrides
            .and_then(|v| v.timeout_secs)
            .or_else(|| {
                std::env::var("IBKR_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
            })
            .or_else(|| ibkr.as_ref().and_then(|v| v.timeout_secs)),
    };

    Ok(config)
}

pub(crate) fn has_ibkr_configuration_hint() -> bool {
    let env_present = [
        "IBKR_ACCOUNT",
        "IBKR_HOST",
        "IBKR_PORT",
        "IBKR_CLIENT_ID",
        "IBKR_MARKET_DATA_TYPE",
        "IBKR_TIMEOUT_SECS",
    ]
    .iter()
    .any(|key| {
        std::env::var(key)
            .ok()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
    });
    if env_present {
        return true;
    }

    match read_inventory_file() {
        Ok(Some(inv)) => inv
            .ibkr
            .map(|ibkr| {
                normalized(ibkr.account.as_deref()).is_some()
                    || normalized(ibkr.host.as_deref()).is_some()
                    || ibkr.port.is_some()
                    || ibkr.client_id.is_some()
                    || ibkr.market_data_type.is_some()
                    || ibkr.timeout_secs.is_some()
            })
            .unwrap_or(false),
        _ => false,
    }
}

fn normalized(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn inventory_path() -> PathBuf {
    if let Ok(path) = std::env::var("ELI_INV_PATH") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".config")
        .join("eli")
        .join("inv.toml")
}

fn read_inventory_file() -> std::result::Result<Option<InventoryFile>, String> {
    let path = inventory_path();
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("read inventory file {} failed: {e}", path.display()))?;
    let parsed = toml::from_str::<InventoryFile>(&raw)
        .map_err(|e| format!("parse inventory file {} failed: {e}", path.display()))?;
    Ok(Some(parsed))
}

fn read_secret_file(path: PathBuf) -> std::result::Result<String, String> {
    std::fs::read_to_string(&path)
        .map_err(|e| format!("read secret file {} failed: {e}", path.display()))
        .and_then(|v| {
            if v.trim().is_empty() {
                Err(format!("secret file {} is empty", path.display()))
            } else {
                Ok(v)
            }
        })
}
