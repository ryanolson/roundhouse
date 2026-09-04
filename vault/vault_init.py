"""
Vault initialization module for AgenticTA.

Import this at the start of your application to enable automatic token renewal.

Usage:
    # In your main app file (gradioUI.py, appgraph.py, etc.)
    from vault import vault_init  # That's it!

Or explicit:
    from vault.vault_init import initialize_vault
    initialize_vault()
"""

import os
import logging

logger = logging.getLogger(__name__)

_initialized = False


def initialize_vault():
    """
    Initialize Vault token auto-renewal.
    
    Safe to call multiple times (only initializes once).
    Fails gracefully if Vault is not configured.
    """
    global _initialized
    
    if _initialized:
        logger.debug("Vault already initialized")
        return
    
    # Log Vault status prominently
    try:
        from vault import log_vault_status
        log_vault_status()
    except ImportError:
        # If vault module not available, log manually
        vault_token = os.getenv('VAULT_TOKEN')
        print("\n" + "=" * 70)
        if vault_token:
            print("🔐 SECRETS MANAGEMENT: Using HashiCorp Vault")
        else:
            print("⚠️  SECRETS MANAGEMENT: Vault NOT available")
            print("⚠️  Using environment variables from .env file")
            print("⚠️  This is INSECURE for production!")
        print("=" * 70 + "\n")
    
    # Check if Vault is configured
    vault_addr = os.getenv('VAULT_ADDR')
    vault_token = os.getenv('VAULT_TOKEN')
    
    if not vault_addr or not vault_token:
        logger.info("Vault not configured - using .env fallback")
        _initialized = True  # Mark as initialized so we don't show the banner again
        return
    
    try:
        from vault import start_token_manager, get_token_manager
        
        # Check if already running
        try:
            manager = get_token_manager(auto_start=False)
            if manager._running:
                logger.info("Vault TokenManager already running")
                _initialized = True
                return
        except:
            pass
        
        # Start token renewal
        manager = start_token_manager(
            check_interval=300,       # Check every 5 minutes
            renew_threshold=7200,     # Renew when < 2 hours left
            on_failure=_on_renewal_failure,
            on_renewal=_on_renewal_success
        )
        
        status = manager.get_status()
        ttl_hours = status['token_ttl_hours']
        
        logger.info(
            f"✅ Vault token auto-renewal enabled "
            f"(TTL: {ttl_hours:.1f}h, checks every 5m)"
        )
        
        _initialized = True
        
    except ImportError:
        logger.warning("Vault module not available - install with: pip install hvac")
    except Exception as e:
        logger.warning(f"⚠️  Could not start Vault auto-renewal: {e}")
        logger.info("Application will continue with .env fallback")


def _on_renewal_success(new_ttl):
    """Callback for successful token renewal."""
    hours = new_ttl / 3600
    logger.info(f"✅ Vault token renewed successfully (TTL: {hours:.1f}h)")


def _on_renewal_failure(error):
    """Callback for token renewal failure."""
    logger.error(f"❌ Vault token renewal failed: {error}")
    logger.error("Application may lose Vault access when token expires!")


# Auto-initialize when module is imported
initialize_vault()

