# 🔄 Seamless Vault Switching Guide

AgenticTA supports **seamless switching** between `.env` and Vault configurations.

---

## 🎯 Quick Reference

```bash
# Start WITHOUT Vault (uses .env)
make up

# Start WITH Vault (local dev)
make up-with-vault
make vault-migrate  # One-time: migrate secrets
make gradio         # Start UI

# Stop (automatically cleans up vault if running)
make down
```

---

## 📋 Two Modes

### Mode 1: Without Vault (Default)

**Best for**: Fast local development, testing, demo

```bash
make up
make gradio
```

- ✅ Fast startup (~5 seconds)
- ✅ No extra services
- ✅ Uses `.env` file
- ✅ Simple and straightforward

**When secrets are loaded:**
```
⚠️  VAULT_TOKEN not set. AgenticTA will use .env fallback.
```

---

### Mode 2: With Vault (Development)

**Best for**: Testing vault integration, team development

```bash
make up-with-vault   # Starts AgenticTA + vault-dev
make vault-migrate   # One-time: copy secrets from .env
make vault-check     # Verify secrets
make gradio          # Start UI
```

- ✅ Tests vault integration
- ✅ Automatic secret loading
- ✅ Matches production setup
- ⚠️  Slower startup (~10 seconds)

**When secrets are loaded:**
```
✅ Vault integration is working correctly!
```

---

## 🔄 Switching Between Modes

### Switch from .env to Vault:

```bash
make down              # Stop current services
make up-with-vault     # Start with Vault
make vault-migrate     # Migrate secrets (first time only)
make gradio            # Start UI
```

### Switch from Vault to .env:

```bash
make down              # Stop current services (auto-removes vault)
make up                # Start without Vault
make gradio            # Start UI
```

**That's it!** No manual cleanup needed. `make down` automatically removes vault if it's running.

---

## 🏗️ Technical Details

### How It Works

1. **Default (`make up`)**: Uses `docker-compose.yml` only
   - No vault service
   - AgenticTA loads secrets from `.env`

2. **With Vault (`make up-with-vault`)**: Uses compose override
   ```bash
   docker compose -f docker-compose.yml \
                  -f docker-compose.vault-local.yml up -d
   ```
   - Adds vault-dev service
   - Injects `VAULT_ADDR`, `VAULT_TOKEN` env vars
   - AgenticTA auto-detects and loads from Vault

3. **Cleanup (`make down`)**: Intelligent cleanup
   - Stops main services
   - Auto-detects and removes vault-dev if present
   - Removes vault network

### Configuration Files

- `docker-compose.yml` - Main services (always used)
- `docker-compose.vault-local.yml` - Vault overlay (optional)
- `.env` - Your secrets (always present)

---

## 🎓 Common Workflows

### Daily Development (no vault)

```bash
make up
make gradio
# ... develop ...
make down
```

### Testing Vault Integration

```bash
make up-with-vault
make vault-migrate     # First time only
make vault-check       # Verify secrets
make test              # Run tests
make down
```

### Full Reset

```bash
make down
make clean             # Remove all volumes
make up-with-vault
make vault-migrate     # Re-migrate secrets
make gradio
```

---

## 🚀 Production Deployment

For production, use NVIDIA's Vault servers (see `VAULT_TIERS.md` in this directory):

```bash
# Staging
docker compose -f docker-compose.yml \
               -f docker-compose.vault-staging.yml up -d

# Production
make deploy-prod   # Enforces VAULT_TOKEN requirement
```

---

## 🔍 Debugging

### Check which mode you're in:

```bash
docker compose ps | grep vault-dev
# If found: Vault mode
# If not found: .env mode
```

### View Vault logs:

```bash
docker logs agenticta-vault-dev
```

### Verify secrets loading:

```bash
make vault-check       # Check secrets in vault
make test              # Run unit tests
```

### Manual vault access:

```bash
docker exec -it agenticta-vault-dev vault kv list secret/agenticta
```

---

## 💡 Tips

1. **Default to .env for speed**: Use vault only when testing vault integration
2. **No cleanup needed**: `make down` handles everything
3. **Secrets persist**: Vault data is in Docker volumes (survives restarts)
4. **Network auto-created**: vault-dev network created automatically
5. **One-time migration**: Run `make vault-migrate` only once

---

## ❓ FAQ

**Q: Do I need to migrate secrets every time?**
A: No, only once. Vault data persists in Docker volumes.

**Q: What if vault is slow to start?**
A: The `up-with-vault` target includes a 5-second wait. Adjust if needed.

**Q: Can I use both modes simultaneously?**
A: No, choose one. Use `make down` then `make up` or `make up-with-vault`.

**Q: What happens to .env when using Vault?**
A: It remains as a fallback. If Vault fails, AgenticTA uses .env.

**Q: How do I know which mode is active?**
A: Check container list: `docker compose ps | grep vault-dev`

---

## 📚 Related Documentation

- `VAULT_TIERS.md` - Local, Staging, Production vault servers
- `VAULT_SAFETY.md` - Production safety checks
- `../QUICKSTART.md` - Quick start guide for all modes
- `../SETUP_GUIDE.md` - Detailed setup instructions

