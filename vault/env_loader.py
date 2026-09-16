"""
Environment loader for Vault configuration.
Automatically detects and loads Vault configuration from multiple sources.
"""

import os
from pathlib import Path
from typing import Dict, Optional
import logging

logger = logging.getLogger(__name__)


def detect_vault_environment() -> str:
    """
    Detect which Vault environment is configured.
    
    Returns:
        'local', 'staging', 'production', or 'unknown'
    """
    vault_addr = os.getenv('VAULT_ADDR', '')
    
    if 'localhost' in vault_addr or '127.0.0.1' in vault_addr:
        return 'local'
    elif 'stg.internal.vault.nvidia.com' in vault_addr:
        return 'staging'
    elif 'internal.vault.nvidia.com' in vault_addr and 'stg' not in vault_addr:
        return 'production'
    else:
        return 'unknown'


def load_vault_env() -> Dict[str, str]:
    """
    Load Vault environment configuration from multiple sources.
    
    Priority (highest to lowest):
    1. Environment variables already set
    2. .env.vault-local file
    3. .env file
    4. Default values
    
    Returns:
        Dictionary with Vault configuration
    """
    config = {}
    
    # Find project root (where .env files are)
    current_dir = Path(__file__).resolve().parent
    project_root = current_dir.parent
    
    # Try to load .env.vault-local first
    vault_local_env = project_root / '.env.vault-local'
    if vault_local_env.exists():
        logger.debug(f"Loading Vault config from: {vault_local_env}")
        config.update(_load_env_file(vault_local_env))
    
    # Then try .env
    env_file = project_root / '.env'
    if env_file.exists():
        logger.debug(f"Loading Vault config from: {env_file}")
        config.update(_load_env_file(env_file))
    
    # Override with actual environment variables
    if os.getenv('VAULT_ADDR'):
        config['VAULT_ADDR'] = os.getenv('VAULT_ADDR')
    if os.getenv('VAULT_TOKEN'):
        config['VAULT_TOKEN'] = os.getenv('VAULT_TOKEN')
    if os.getenv('VAULT_NAMESPACE') is not None:  # Can be empty string
        config['VAULT_NAMESPACE'] = os.getenv('VAULT_NAMESPACE')
    
    # DO NOT set defaults - let Vault configuration be explicit
    # Setting defaults causes unexpected behavior when Vault is not configured
    
    # Only set environment variables if they were explicitly configured
    for key, value in config.items():
        if key not in os.environ or not os.environ[key]:
            os.environ[key] = value
    
    env_type = detect_vault_environment()
    logger.info(f"Vault environment: {env_type}")
    logger.debug(f"Vault address: {config.get('VAULT_ADDR')}")
    
    return config


def _load_env_file(filepath: Path) -> Dict[str, str]:
    """
    Load environment variables from a file.
    
    Handles both formats:
    - KEY=value
    - export KEY='value'
    
    Args:
        filepath: Path to env file
        
    Returns:
        Dictionary of environment variables
    """
    config = {}
    
    try:
        with open(filepath, 'r') as f:
            for line in f:
                line = line.strip()
                
                # Skip comments and empty lines
                if not line or line.startswith('#'):
                    continue
                
                # Handle export statements
                if line.startswith('export '):
                    line = line[7:]  # Remove 'export '
                
                # Parse KEY=value
                if '=' in line:
                    key, value = line.split('=', 1)
                    key = key.strip()
                    value = value.strip()
                    
                    # Remove quotes
                    if value.startswith(("'", '"')) and value.endswith(("'", '"')):
                        value = value[1:-1]
                    
                    # Only load Vault-related variables
                    if key.startswith('VAULT_'):
                        config[key] = value
    
    except Exception as e:
        logger.warning(f"Failed to load {filepath}: {e}")
    
    return config


def ensure_vault_config():
    """
    Ensure Vault configuration is loaded.
    Call this explicitly when needed (not automatic).
    """
    load_vault_env()


# DO NOT auto-load on import - causes unexpected side effects
# Call ensure_vault_config() explicitly if needed

