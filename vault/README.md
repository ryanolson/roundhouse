# Vault Integration Module

This module provides secure secrets management for AgenticTA using HashiCorp Vault.

## Quick Start

```python
from vault import get_secrets_config

# Load all secrets (auto-detects Vault availability)
secrets = get_secrets_config()

# Get specific secrets
nvidia_key = secrets.get('NVIDIA_API_KEY')
hf_token = secrets.get('HF_TOKEN')
```

## Module Structure

- **`client.py`** - Vault client wrapper with caching and error handling
- **`config.py`** - Centralized secrets configuration with fallback support
- **`__init__.py`** - Public API exports

## Features

- ✅ Secure secret storage in HashiCorp Vault
- ✅ Intelligent caching (1-hour TTL)
- ✅ Automatic fallback to environment variables
- ✅ Comprehensive error handling
- ✅ Token management and renewal

## Usage Examples

### Basic Usage

```python
from vault import get_secrets_config

secrets = get_secrets_config()

# Get secret (returns None if not found)
api_key = secrets.get('NVIDIA_API_KEY')

# Get all secrets
all_secrets = secrets.get_all()
```

### Using Convenience Functions

```python
from vault import (
    get_nvidia_api_key,
    get_hf_token,
)

# Required secrets (raise error if missing)
nvidia_key = get_nvidia_api_key()

# Optional secrets (return None if missing)
hf_token = get_hf_token()

# Note: ASTRA_TOKEN is deprecated (March 2026).  Use INFERENCE_API_KEY instead.
# inference_key = get_secret('INFERENCE_API_KEY')
```

### Direct Vault Access

```python
from vault import get_vault_client

# Get Vault client
vault = get_vault_client()

# Read secret
secret = vault.get_secret('agenticta/api-keys', 'nvidia_api_key')

# Write secret
vault.set_secret('agenticta/api-keys', {
    'nvidia_api_key': 'new-value'
})

# List secrets
secrets = vault.list_secrets('agenticta')
```

## Scripts and Documentation

All scripts, migration tools, and comprehensive documentation are in:
```
scripts/vault/
```

See [`scripts/vault/README.md`](../scripts/vault/README.md) for:
- Migration guide
- Health check tools
- Troubleshooting
- Best practices

## Environment Variables

Required:
- `VAULT_ADDR` - Vault server address (default: https://stg.internal.vault.nvidia.com)
- `VAULT_TOKEN` - Authentication token
- `VAULT_NAMESPACE` - Vault namespace (default: wwfo-self-ta)

Optional (fallback values):
- `NVIDIA_API_KEY`
- `INFERENCE_API_KEY`
- `HF_TOKEN`
- `ASTRA_TOKEN` (deprecated — kept for backward compatibility)
- `DATADOG_TOKEN`
- `DATADOG_EMBEDDING_API_TOKEN`

## Configuration

Set environment variables in your `.env` file:

```bash
# Vault Configuration
VAULT_ADDR=https://stg.internal.vault.nvidia.com
VAULT_NAMESPACE=wwfo-self-ta
VAULT_TOKEN=hvs.YOUR_TOKEN_HERE

# Optional fallbacks
NVIDIA_API_KEY=fallback-if-vault-unavailable
```

See [`scripts/vault/env.template`](../scripts/vault/env.template) for a complete template.

## Testing

Verify vault integration:

```bash
make vault-check    # Check secrets in vault
make test           # Run automated tests
```

## Support

- **Quick Start**: [`scripts/vault/QUICKSTART.md`](../scripts/vault/QUICKSTART.md)
- **Best Practices**: [`scripts/vault/VAULT_BEST_PRACTICES.md`](../scripts/vault/VAULT_BEST_PRACTICES.md)
- **Troubleshooting**: Run `python scripts/vault/vault_health_check.py`

## Architecture

```
vault/
├── __init__.py       # Public API
├── client.py         # VaultClient class
├── config.py         # SecretsConfig class
└── README.md         # This file
```

## Security

- ⚠️ Never commit `VAULT_TOKEN` to git
- ⚠️ Add `.env` to `.gitignore`
- ⚠️ Rotate secrets regularly (90 days for API keys)
- ⚠️ Use read-only tokens when possible
- ⚠️ Monitor Vault access logs

---

**Version**: 1.0.0  
**Last Updated**: November 7, 2025

