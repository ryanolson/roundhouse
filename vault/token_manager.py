"""
Token management for production Vault usage (no fallback).

This module handles automatic token renewal and monitoring for
production environments where .env fallback is not available.
"""

import threading
import time
import logging
from typing import Optional, Callable
from datetime import datetime
from vault.client import get_vault_client, VaultClient

logger = logging.getLogger(__name__)


class TokenManager:
    """
    Manages Vault token lifecycle with automatic renewal.
    
    Features:
    - Automatic token renewal
    - Health monitoring
    - Alert callbacks for failures
    - Graceful shutdown
    
    Example:
        >>> from vault.token_manager import TokenManager
        >>> 
        >>> def on_renewal_failure(error):
        ...     send_alert(f"CRITICAL: Vault token renewal failed: {error}")
        >>> 
        >>> manager = TokenManager(
        ...     check_interval=300,  # Check every 5 minutes
        ...     renew_threshold=7200,  # Renew when < 2 hours left
        ...     on_failure=on_renewal_failure
        ... )
        >>> manager.start()
    """
    
    def __init__(
        self,
        vault_client: Optional[VaultClient] = None,
        check_interval: int = 300,  # 5 minutes
        renew_threshold: int = 7200,  # 2 hours
        on_failure: Optional[Callable[[str], None]] = None,
        on_renewal: Optional[Callable[[int], None]] = None
    ):
        """
        Initialize Token Manager.
        
        Args:
            vault_client: VaultClient instance (uses singleton if None)
            check_interval: How often to check token status (seconds)
            renew_threshold: Renew when TTL below this (seconds)
            on_failure: Callback function called on renewal failure
            on_renewal: Callback function called on successful renewal
        """
        self.vault = vault_client or get_vault_client()
        self.check_interval = check_interval
        self.renew_threshold = renew_threshold
        self.on_failure = on_failure
        self.on_renewal = on_renewal
        
        self._running = False
        self._thread: Optional[threading.Thread] = None
        self._last_check: Optional[datetime] = None
        self._last_renewal: Optional[datetime] = None
        self._renewal_count = 0
        self._failure_count = 0
    
    def start(self):
        """Start the token renewal daemon."""
        if self._running:
            logger.warning("Token manager already running")
            return
        
        self._running = True
        self._thread = threading.Thread(
            target=self._renewal_loop,
            name="VaultTokenManager",
            daemon=True
        )
        self._thread.start()
        logger.info(
            f"Token manager started "
            f"(check_interval={self.check_interval}s, "
            f"renew_threshold={self.renew_threshold}s)"
        )
    
    def stop(self):
        """Stop the token renewal daemon."""
        if not self._running:
            return
        
        self._running = False
        if self._thread:
            self._thread.join(timeout=5)
        logger.info("Token manager stopped")
    
    def get_status(self) -> dict:
        """
        Get current status of token manager.
        
        Returns:
            Dictionary with status information
        """
        try:
            token_info = self.vault.get_token_info()
            ttl = token_info.get('ttl', 0)
        except Exception as e:
            ttl = None
            logger.error(f"Failed to get token info: {e}")
        
        return {
            'running': self._running,
            'last_check': self._last_check.isoformat() if self._last_check else None,
            'last_renewal': self._last_renewal.isoformat() if self._last_renewal else None,
            'renewal_count': self._renewal_count,
            'failure_count': self._failure_count,
            'token_ttl': ttl,
            'token_ttl_hours': ttl / 3600 if ttl else None,
            'needs_renewal': ttl < self.renew_threshold if ttl else True,
        }
    
    def force_renewal(self) -> bool:
        """
        Force immediate token renewal.
        
        Returns:
            True if renewal successful, False otherwise
        """
        return self._attempt_renewal()
    
    def _renewal_loop(self):
        """Main renewal loop (runs in background thread)."""
        logger.info("Token renewal loop started")
        
        while self._running:
            try:
                self._check_and_renew()
            except Exception as e:
                logger.error(f"Error in renewal loop: {e}", exc_info=True)
                self._failure_count += 1
                
                if self.on_failure:
                    try:
                        self.on_failure(str(e))
                    except Exception as callback_error:
                        logger.error(f"Failure callback error: {callback_error}")
            
            # Sleep in small increments to allow quick shutdown
            for _ in range(self.check_interval):
                if not self._running:
                    break
                time.sleep(1)
        
        logger.info("Token renewal loop stopped")
    
    def _check_and_renew(self):
        """Check token TTL and renew if needed."""
        self._last_check = datetime.now()
        
        try:
            token_info = self.vault.get_token_info()
            ttl = token_info.get('ttl', 0)
            ttl_hours = ttl / 3600
            
            logger.debug(f"Token check: TTL={ttl}s ({ttl_hours:.1f}h)")
            
            if ttl < self.renew_threshold:
                logger.info(
                    f"Token TTL ({ttl}s) below threshold ({self.renew_threshold}s), "
                    f"attempting renewal..."
                )
                self._attempt_renewal()
            else:
                logger.debug(f"Token healthy: {ttl_hours:.1f} hours remaining")
                
        except Exception as e:
            logger.error(f"Failed to check token status: {e}")
            raise
    
    def _attempt_renewal(self) -> bool:
        """
        Attempt to renew the token.
        
        Returns:
            True if renewal successful, False otherwise
        """
        try:
            success = self.vault.renew_token()
            
            if success:
                self._last_renewal = datetime.now()
                self._renewal_count += 1
                
                # Get new TTL
                token_info = self.vault.get_token_info()
                new_ttl = token_info.get('ttl', 0)
                new_ttl_hours = new_ttl / 3600
                
                logger.info(
                    f"✓ Token renewed successfully "
                    f"(#{self._renewal_count}, new TTL: {new_ttl_hours:.1f}h)"
                )
                
                if self.on_renewal:
                    try:
                        self.on_renewal(new_ttl)
                    except Exception as callback_error:
                        logger.error(f"Renewal callback error: {callback_error}")
                
                return True
            else:
                self._failure_count += 1
                error_msg = "Token renewal returned False"
                logger.error(f"✗ {error_msg}")
                
                if self.on_failure:
                    self.on_failure(error_msg)
                
                return False
                
        except Exception as e:
            self._failure_count += 1
            error_msg = f"Token renewal exception: {e}"
            logger.error(f"✗ {error_msg}", exc_info=True)
            
            if self.on_failure:
                self.on_failure(error_msg)
            
            return False


# Global singleton instance
_token_manager: Optional[TokenManager] = None


def get_token_manager(
    vault_client: Optional[VaultClient] = None,
    check_interval: int = 300,
    renew_threshold: int = 7200,
    on_failure: Optional[Callable[[str], None]] = None,
    on_renewal: Optional[Callable[[int], None]] = None,
    auto_start: bool = True
) -> TokenManager:
    """
    Get or create global TokenManager instance.
    
    Args:
        vault_client: VaultClient instance
        check_interval: Check interval in seconds (default: 5 minutes)
        renew_threshold: Renew threshold in seconds (default: 2 hours)
        on_failure: Callback for renewal failures
        on_renewal: Callback for successful renewals
        auto_start: Automatically start the manager
        
    Returns:
        TokenManager instance
    """
    global _token_manager
    
    if _token_manager is None:
        _token_manager = TokenManager(
            vault_client=vault_client,
            check_interval=check_interval,
            renew_threshold=renew_threshold,
            on_failure=on_failure,
            on_renewal=on_renewal
        )
        
        if auto_start:
            _token_manager.start()
    
    return _token_manager


def start_token_manager(**kwargs):
    """
    Convenience function to start token manager.
    
    Args:
        **kwargs: Arguments passed to get_token_manager()
    
    Example:
        >>> from vault.token_manager import start_token_manager
        >>> 
        >>> start_token_manager(
        ...     check_interval=300,
        ...     renew_threshold=7200
        ... )
    """
    manager = get_token_manager(**kwargs)
    if not manager._running:
        manager.start()
    return manager

