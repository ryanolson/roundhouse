"""
Centralized secrets configuration for AgenticTA.
Loads secrets from Vault with .env fallback.
"""

import os
from typing import Dict, Optional
from vault.client import get_secret_with_fallback, get_vault_client
import logging

logger = logging.getLogger(__name__)


class SecretsConfig:
    """Centralized secrets configuration."""
    
    # Secret paths in Vault
    API_KEYS_PATH = "agenticta/api-keys"
    AUTH_TOKENS_PATH = "agenticta/auth-tokens"
    OBSERVABILITY_PATH = "agenticta/observability"
    
    # Secrets configuration (env_var_name, vault_path, vault_key, required, description)
    # To add a new secret: just add a tuple here!
    SECRETS_REGISTRY = [
        ('NVIDIA_API_KEY', API_KEYS_PATH, 'nvidia_api_key', True, 'NVIDIA API'),
        ('INFERENCE_API_KEY', API_KEYS_PATH, 'inference_api_key', True, 'NVIDIA Inference Hub'),
        ('HF_TOKEN', API_KEYS_PATH, 'hf_token', False, 'HuggingFace'),
        ('ASTRA_TOKEN', AUTH_TOKENS_PATH, 'astra_token', False, 'Astra (deprecated)'),
        ('DATADOG_EMBEDDING_API_TOKEN', OBSERVABILITY_PATH, 'datadog_embedding_api_token', False, 'Datadog'),
    ]
    
    def __init__(self, use_vault: bool = True):
        """
        Initialize secrets configuration.
        
        Args:
            use_vault: Whether to use Vault (False = .env only)
        """
        self.use_vault = use_vault
        self._secrets: Dict[str, Optional[str]] = {}
        self._load_secrets()
    
    def _load_secrets(self):
        """Load all secrets from Vault or environment."""
        if self.use_vault:
            self._load_from_vault()
        else:
            self._load_from_env()
    
    def _load_from_vault(self):
        """Load secrets from Vault with .env fallback."""
        # Check if Vault is actually configured
        vault_token = os.getenv('VAULT_TOKEN')
        environment = os.getenv('ENVIRONMENT', 'development').lower()
        require_vault = os.getenv('REQUIRE_VAULT', '').lower() in ('true', '1', 'yes')
        
        # Production safety check
        if (environment == 'production' or require_vault) and not vault_token:
            error_msg = (
                "❌ PRODUCTION ERROR: VAULT_TOKEN not set!\n"
                f"   ENVIRONMENT: {environment}\n"
                f"   REQUIRE_VAULT: {require_vault}\n"
                "   Production deployments MUST use Vault for security.\n"
                "   Set VAULT_TOKEN or set ENVIRONMENT=development for local dev."
            )
            logger.error(error_msg)
            raise ValueError(error_msg)
        
        if not vault_token:
            logger.info("VAULT_TOKEN not set - using environment variables only (development mode)")
            return self._load_from_env()
        
        logger.info("Loading secrets from Vault...")
        
        # Load all secrets using registry configuration
        for env_var, vault_path, vault_key, required, description in self.SECRETS_REGISTRY:
            try:
                self._secrets[env_var] = get_secret_with_fallback(
                    vault_path=vault_path,
                    vault_key=vault_key,
                    env_var=env_var,
                    required=required
                )
            except ValueError as e:
                if required:
                    logger.error(f"Failed to load required secret {env_var}: {e}")
                    raise
                else:
                    logger.debug(f"Optional secret {env_var} ({description}) not found")
        
        # Log loaded secrets (without values!)
        loaded = [k for k, v in self._secrets.items() if v is not None]
        logger.info(f"Loaded {len(loaded)} secrets: {', '.join(loaded)}")
    
    def _load_from_env(self):
        """Load secrets from environment variables only."""
        logger.info("Loading secrets from environment...")
        
        # Load all secrets from registry (only env_var is needed for env loading)
        for env_var, _, _, _, _ in self.SECRETS_REGISTRY:
            value = os.getenv(env_var)
            if value:
                self._secrets[env_var] = value
        
        loaded = [k for k, v in self._secrets.items() if v is not None]
        logger.info(f"Loaded {len(loaded)} secrets from environment")
    
    def get(self, key: str, default: Optional[str] = None) -> Optional[str]:
        """
        Get secret value.
        
        Args:
            key: Secret key name
            default: Default value if not found
            
        Returns:
            Secret value or default
        """
        return self._secrets.get(key, default)
    
    def get_all(self) -> Dict[str, Optional[str]]:
        """
        Get all secrets as dictionary.
        
        Returns:
            Dictionary of all secrets (excluding None values)
        """
        return {k: v for k, v in self._secrets.items() if v is not None}
    
    def reload(self):
        """Reload all secrets from source."""
        self._secrets.clear()
        self._load_secrets()
        logger.info("Secrets reloaded")
    
    def __getitem__(self, key: str) -> Optional[str]:
        """Allow dict-like access."""
        return self._secrets[key]
    
    def __contains__(self, key: str) -> bool:
        """Allow 'in' operator."""
        return key in self._secrets
    
    def __repr__(self) -> str:
        """String representation (without secret values)."""
        keys = list(self._secrets.keys())
        return f"SecretsConfig(use_vault={self.use_vault}, secrets={keys})"


# Global config instance
_secrets_config: Optional[SecretsConfig] = None


def get_secrets_config(use_vault: Optional[bool] = None, force_reload: bool = False) -> SecretsConfig:
    """
    Get or create global secrets configuration.
    
    Args:
        use_vault: Override vault usage (None = auto-detect)
        force_reload: Force reload secrets
        
    Returns:
        SecretsConfig instance
    """
    global _secrets_config
    
    if _secrets_config is None or force_reload:
        # Auto-detect: use Vault if VAULT_TOKEN is set
        if use_vault is None:
            vault_token = os.getenv('VAULT_TOKEN')
            use_vault = bool(vault_token)
            
            # Log prominently about Vault status
            if use_vault:
                logger.info("=" * 70)
                logger.info("🔐 SECRETS MANAGEMENT: Using HashiCorp Vault")
                logger.info("=" * 70)
            else:
                logger.warning("=" * 70)
                logger.warning("⚠️  SECRETS MANAGEMENT: Vault NOT available")
                logger.warning("⚠️  Using environment variables from .env file")
                logger.warning("⚠️  This is INSECURE for production!")
                logger.warning("=" * 70)
                logger.warning("To enable Vault:")
                logger.warning("  1. Stop services: make down")
                logger.warning("  2. Start with Vault: make up-with-vault")
                logger.warning("  3. Migrate secrets: make vault-migrate")
                logger.warning("=" * 70)
        else:
            if use_vault:
                logger.info(f"🔐 Vault explicitly enabled")
            else:
                logger.warning(f"⚠️  Vault explicitly disabled - using environment variables")
        
        _secrets_config = SecretsConfig(use_vault=use_vault)
    
    return _secrets_config


# Convenience functions for common secrets
def get_nvidia_api_key() -> str:
    """Get NVIDIA API key (required)."""
    config = get_secrets_config()
    key = config.get('NVIDIA_API_KEY')
    if not key:
        raise ValueError("NVIDIA_API_KEY not configured")
    return key


def get_hf_token() -> Optional[str]:
    """Get Hugging Face token (optional)."""
    config = get_secrets_config()
    return config.get('HF_TOKEN')


def get_astra_token() -> Optional[str]:
    """Get Astra token (optional)."""
    config = get_secrets_config()
    return config.get('ASTRA_TOKEN')


def get_datadog_embedding_token() -> Optional[str]:
    """Get Datadog embedding API token (optional)."""
    config = get_secrets_config()
    return config.get('DATADOG_EMBEDDING_API_TOKEN')


