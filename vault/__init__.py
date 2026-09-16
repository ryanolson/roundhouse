"""
Vault integration for AgenticTA.

This module provides secure secrets management using HashiCorp Vault.
Secrets can be read from Vault with automatic fallback to environment variables.

Quick Start:
    >>> from vault import get_secrets_config
    >>> secrets = get_secrets_config()
    >>> api_key = secrets.get('NVIDIA_API_KEY')

Production (No Fallback):
    >>> from vault import start_token_manager, get_secrets_config
    >>> # Start automatic token renewal
    >>> start_token_manager(check_interval=300, renew_threshold=7200)
    >>> # Load secrets from Vault only
    >>> secrets = get_secrets_config()

For more details, see the documentation in scripts/vault/
"""

import os
import logging

from vault.client import VaultClient, get_vault_client, get_secret_with_fallback
from vault.config import (
    SecretsConfig,
    get_secrets_config,
    get_nvidia_api_key,
    get_hf_token,
    get_astra_token,
    get_datadog_embedding_token,
)
from vault.token_manager import TokenManager, get_token_manager, start_token_manager

logger = logging.getLogger(__name__)


def log_vault_status():
    """
    Log the current Vault configuration status.
    
    This function should be called at application startup to make it clear
    whether Vault is being used for secrets management.
    """
    vault_token = os.getenv('VAULT_TOKEN')
    
    print("\n" + "=" * 70)
    if vault_token:
        print("🔐 SECRETS MANAGEMENT: Using HashiCorp Vault")
        print("=" * 70)
        logger.info("Vault is enabled for secrets management")
    else:
        print("⚠️  SECRETS MANAGEMENT: Vault NOT available")
        print("⚠️  Using environment variables from .env file")
        print("⚠️  This is INSECURE for production!")
        print("=" * 70)
        print("To enable Vault:")
        print("  1. Stop services: make down")
        print("  2. Start with Vault: make up-with-vault")
        print("  3. Migrate secrets: make vault-migrate")
        print("=" * 70)
        logger.warning("Vault is NOT enabled - using environment variables")
    print()


# ============================================================================
# Simple Public API (works just like os.getenv!)
# ============================================================================

def get_secret(key: str, default=None, required=True):
    """
    Get a secret value - works just like os.getenv() but with vault support.
    
    Automatically handles:
    - Vault connection (if VAULT_TOKEN is set)
    - Fallback to environment variables (even for unknown keys)
    - Caching for performance
    
    Args:
        key: Secret name (e.g., 'NVIDIA_API_KEY', 'ANTHROPIC_API_KEY', etc.)
        default: Default value if secret not found (implies required=False)
        required: Raise error if secret not found (default: True)
        
    Returns:
        Secret value or default
        
    Raises:
        ValueError: If required=True and secret not found
        
    Examples:
        >>> from vault import get_secret
        >>> # Required by default (raises if missing)
        >>> api_key = get_secret('NVIDIA_API_KEY')
        >>> 
        >>> # Works with any key (checks env vars automatically)
        >>> anthropic = get_secret('ANTHROPIC_API_KEY')
        >>> 
        >>> # Optional with default (implies required=False)
        >>> token = get_secret('HF_TOKEN', default='')
        >>> 
        >>> # Explicitly optional (no error if missing)
        >>> optional = get_secret('OPTIONAL_KEY', required=False)
    """
    # If default is provided, implicitly make it optional (unless explicitly required)
    if default is not None and required is True:
        required = False
    
    # Try to get from SecretsConfig (vault or predefined env vars)
    config = get_secrets_config()
    value = config.get(key)
    
    # If not found and no vault, try os.getenv as fallback for unknown keys
    if value is None:
        value = os.getenv(key)
    
    # Use default if still not found
    if value is None:
        value = default
    
    if required and value is None:
        raise ValueError(f"Required secret '{key}' not found in Vault or environment")
    
    return value


def require_secret(key: str):
    """
    Get a required secret - raises error if not found.
    
    Args:
        key: Secret name
        
    Returns:
        Secret value (never None)
        
    Raises:
        ValueError: If secret not found
        
    Example:
        >>> from vault import require_secret
        >>> api_key = require_secret('NVIDIA_API_KEY')
    """
    return get_secret(key, required=True)


def has_secret(key: str) -> bool:
    """
    Check if a secret exists.
    
    Args:
        key: Secret name
        
    Returns:
        True if secret exists, False otherwise
        
    Example:
        >>> from vault import has_secret
        >>> if has_secret('DATADOG_TOKEN'):
        ...     setup_datadog()
    """
    return get_secret(key) is not None


def get_all_secrets() -> dict:
    """
    Get all configured secrets as a dictionary.
    
    Returns:
        Dictionary of all secrets (excluding None values)
        
    Example:
        >>> from vault import get_all_secrets
        >>> secrets = get_all_secrets()
        >>> print(secrets.keys())
    """
    config = get_secrets_config()
    return config.get_all()


__all__ = [
    # Simple API (recommended for users)
    'get_secret',
    'require_secret',
    'has_secret',
    'get_all_secrets',
    # Client (advanced usage)
    'VaultClient',
    'get_vault_client',
    'get_secret_with_fallback',
    # Config (advanced usage)
    'SecretsConfig',
    'get_secrets_config',
    # Convenience functions (legacy)
    'get_nvidia_api_key',
    'get_hf_token',
    'get_astra_token',
    'get_datadog_embedding_token',
    # Token Management (production)
    'TokenManager',
    'get_token_manager',
    'start_token_manager',
    # Status
    'log_vault_status',
]

__version__ = '1.0.0'

