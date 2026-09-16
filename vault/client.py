"""
Vault client wrapper for AgenticTA.
Handles secret retrieval with caching, error handling, and fallbacks.
"""

import os
import hvac
import warnings
from typing import Optional, Dict, Any
from datetime import datetime, timedelta
import logging

# DO NOT auto-load Vault configuration - causes unexpected side effects
# Vault configuration should be explicit via environment variables
# If you need to load from .env files, call ensure_vault_config() manually

logger = logging.getLogger(__name__)


class VaultClient:
    """Wrapper for HashiCorp Vault client with best practices."""
    
    def __init__(
        self,
        vault_addr: Optional[str] = None,
        vault_token: Optional[str] = None,
        vault_namespace: Optional[str] = None,
        mount_point: Optional[str] = None,
        cache_ttl: int = 3600
    ):
        """
        Initialize Vault client.
        
        Args:
            vault_addr: Vault server address
            vault_token: Authentication token
            vault_namespace: Vault namespace
            mount_point: KV secrets engine mount point (default: 'secret')
            cache_ttl: Cache TTL in seconds (default: 1 hour)
        """
        self.vault_addr = vault_addr or os.getenv(
            'VAULT_ADDR', 
            'https://stg.internal.vault.nvidia.com'
        )
        self.vault_token = vault_token or os.getenv('VAULT_TOKEN')
        self.vault_namespace = vault_namespace or os.getenv(
            'VAULT_NAMESPACE', 
            'wwfo-self-ta'
        )
        self.mount_point = mount_point or os.getenv('VAULT_MOUNT_POINT', 'secret')
        
        if not self.vault_token:
            raise ValueError("VAULT_TOKEN must be set")
        
        # Initialize client
        self.client = hvac.Client(
            url=self.vault_addr,
            token=self.vault_token,
            namespace=self.vault_namespace
        )
        
        # Initialize cache
        self._cache: Dict[str, tuple] = {}
        self._cache_ttl = cache_ttl
        
        # Verify authentication
        if not self.client.is_authenticated():
            raise ValueError("Failed to authenticate with Vault")
        
        logger.info(f"Vault client initialized: {self.vault_addr}")
        logger.info(f"KV mount point: {self.mount_point}")
        if self.vault_namespace:
            logger.info(f"Namespace: {self.vault_namespace}")
    
    def get_secret(
        self, 
        path: str, 
        key: Optional[str] = None,
        use_cache: bool = True
    ) -> Optional[Any]:
        """
        Get secret from Vault.
        
        Args:
            path: Secret path (e.g., 'agenticta/api-keys')
            key: Specific key within secret (if None, returns all)
            use_cache: Whether to use cached value
            
        Returns:
            Secret value or None if not found
            
        Example:
            >>> vault = VaultClient()
            >>> api_key = vault.get_secret('agenticta/api-keys', 'nvidia_api_key')
            >>> all_keys = vault.get_secret('agenticta/api-keys')
        """
        cache_key = f"{path}:{key}" if key else path
        
        # Check cache first
        if use_cache:
            cached = self._get_from_cache(cache_key)
            if cached is not None:
                logger.debug(f"Cache hit for: {cache_key}")
                return cached
        
        # Read from Vault
        try:
            response = self.client.secrets.kv.v2.read_secret_version(
                mount_point=self.mount_point,
                path=path,
                raise_on_deleted_version=False  # Graceful handling, future-proof for hvac v3.0.0
            )
            secret_data = response['data']['data']
            
            # Cache the full secret
            self._add_to_cache(path, secret_data)
            
            # Return requested key or full secret
            if key:
                value = secret_data.get(key)
                self._add_to_cache(cache_key, value)
                return value
            else:
                return secret_data
                
        except hvac.exceptions.Forbidden as e:
            logger.error(f"Access denied to secret {path}: {e}")
            return None
        except hvac.exceptions.InvalidPath as e:
            logger.error(f"Secret not found {path}: {e}")
            return None
        except Exception as e:
            logger.error(f"Failed to read secret {path}: {e}")
            return None
    
    def set_secret(
        self, 
        path: str, 
        secret: Dict[str, Any],
        clear_cache: bool = True
    ) -> bool:
        """
        Set secret in Vault.
        
        Args:
            path: Secret path
            secret: Secret data as dictionary
            clear_cache: Whether to clear cache after update
            
        Returns:
            True if successful, False otherwise
            
        Example:
            >>> vault = VaultClient()
            >>> vault.set_secret('agenticta/api-keys', {
            ...     'nvidia_api_key': 'nvapi-xxx',
            ...     'updated_at': '2025-11-07'
            ... })
        """
        try:
            self.client.secrets.kv.v2.create_or_update_secret(
                mount_point=self.mount_point,
                path=path,
                secret=secret
            )
            
            if clear_cache:
                self.clear_cache()
            
            logger.info(f"Secret updated: {path}")
            return True
            
        except Exception as e:
            logger.error(f"Failed to set secret {path}: {e}")
            return False
    
    def list_secrets(self, path: str) -> Optional[list]:
        """
        List secrets at path.
        
        Args:
            path: Path to list
            
        Returns:
            List of secret names or None
            
        Example:
            >>> vault = VaultClient()
            >>> secrets = vault.list_secrets('agenticta')
            >>> print(secrets)
            ['api-keys', 'auth-tokens', 'observability']
        """
        try:
            response = self.client.secrets.kv.v2.list_secrets(
                mount_point=self.mount_point,
                path=path
            )
            return response['data']['keys']
        except Exception as e:
            logger.error(f"Failed to list secrets at {path}: {e}")
            return None
    
    def delete_secret(self, path: str, versions: Optional[list] = None) -> bool:
        """
        Delete secret or specific versions.
        
        Args:
            path: Secret path
            versions: List of versions to delete (None = delete all)
            
        Returns:
            True if successful, False otherwise
        """
        try:
            if versions:
                # Delete specific versions
                self.client.secrets.kv.v2.delete_secret_versions(
                    mount_point=self.mount_point,
                    path=path,
                    versions=versions
                )
                logger.info(f"Deleted versions {versions} of secret: {path}")
            else:
                # Delete all versions (metadata delete)
                self.client.secrets.kv.v2.delete_metadata_and_all_versions(
                    mount_point=self.mount_point,
                    path=path
                )
                logger.info(f"Deleted secret and all versions: {path}")
            
            self.clear_cache()
            return True
            
        except Exception as e:
            logger.error(f"Failed to delete secret {path}: {e}")
            return False
    
    def _get_from_cache(self, key: str) -> Optional[Any]:
        """Get value from cache if not expired."""
        if key in self._cache:
            value, timestamp = self._cache[key]
            if datetime.now() - timestamp < timedelta(seconds=self._cache_ttl):
                return value
            else:
                # Expired, remove from cache
                del self._cache[key]
        return None
    
    def _add_to_cache(self, key: str, value: Any):
        """Add value to cache with timestamp."""
        self._cache[key] = (value, datetime.now())
    
    def clear_cache(self):
        """Clear all cached secrets."""
        self._cache.clear()
        logger.info("Secret cache cleared")
    
    def renew_token(self) -> bool:
        """
        Renew Vault token.
        
        Returns:
            True if successful, False otherwise
        """
        try:
            self.client.auth.token.renew_self()
            logger.info("Vault token renewed")
            return True
        except Exception as e:
            logger.error(f"Failed to renew token: {e}")
            return False
    
    def get_token_info(self) -> Optional[Dict[str, Any]]:
        """
        Get information about current token.
        
        Returns:
            Token information dictionary or None
        """
        try:
            response = self.client.auth.token.lookup_self()
            return response['data']
        except Exception as e:
            logger.error(f"Failed to get token info: {e}")
            return None


# Global client instance (singleton pattern)
_vault_client: Optional[VaultClient] = None


def get_vault_client(
    vault_addr: Optional[str] = None,
    vault_token: Optional[str] = None,
    vault_namespace: Optional[str] = None,
    cache_ttl: int = 3600,
    force_new: bool = False
) -> VaultClient:
    """
    Get or create global Vault client instance.
    
    Args:
        vault_addr: Vault server address
        vault_token: Authentication token
        vault_namespace: Vault namespace
        cache_ttl: Cache TTL in seconds
        force_new: Force create new client instance
        
    Returns:
        VaultClient instance
    """
    global _vault_client
    
    if _vault_client is None or force_new:
        _vault_client = VaultClient(
            vault_addr=vault_addr,
            vault_token=vault_token,
            vault_namespace=vault_namespace,
            cache_ttl=cache_ttl
        )
    
    return _vault_client


def get_secret_with_fallback(
    vault_path: str,
    vault_key: str,
    env_var: str,
    required: bool = True
) -> Optional[str]:
    """
    Get secret from Vault with fallback to environment variable.
    
    This is useful during migration period when some environments
    still use .env files.
    
    Args:
        vault_path: Path in Vault
        vault_key: Key within the secret
        env_var: Environment variable name (fallback)
        required: Whether secret is required
        
    Returns:
        Secret value or None
        
    Raises:
        ValueError: If required secret not found
        
    Example:
        >>> api_key = get_secret_with_fallback(
        ...     vault_path='agenticta/api-keys',
        ...     vault_key='nvidia_api_key',
        ...     env_var='NVIDIA_API_KEY',
        ...     required=True
        ... )
    """
    # Try Vault first
    try:
        client = get_vault_client()
        value = client.get_secret(vault_path, vault_key)
        if value:
            logger.debug(f"Loaded {vault_key} from Vault")
            return value
    except Exception as e:
        logger.warning(f"Failed to get {vault_key} from Vault: {e}")
    
    # Fallback to environment variable
    value = os.getenv(env_var)
    if value:
        warning_msg = (
            f"⚠️  VAULT FALLBACK: Using {env_var} from environment variable. "
            f"Vault unavailable or secret not found at {vault_path}/{vault_key}. "
            f"This is insecure for production!"
        )
        logger.warning(warning_msg)
        warnings.warn(warning_msg, UserWarning, stacklevel=2)
        return value
    
    # Not found
    if required:
        raise ValueError(
            f"Required secret not found: {vault_key} "
            f"(tried Vault path '{vault_path}' and env var '{env_var}')"
        )
    
    return None

