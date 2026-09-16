# Vault Safety Mechanisms

AgenticTA has built-in safety checks to prevent accidentally running production without Vault.

## 🛡️ Safety Levels

### Level 1: Development (Default)
```bash
# No special config needed
VAULT_TOKEN=         # Not set
ENVIRONMENT=         # Defaults to 'development'
```

**Behavior:**
- ✅ Uses `.env` file (no warnings)
- ✅ Works offline
- ✅ Fast startup

**When to use:** Local development, testing

---

### Level 2: Staging (Vault Optional, Warnings Enabled)
```bash
ENVIRONMENT=staging
VAULT_TOKEN=your-staging-token
```

**Behavior:**
- ⚠️ If VAULT_TOKEN set: Uses Vault
- ⚠️ If Vault fails: Falls back to `.env` with warnings
- ⚠️ If VAULT_TOKEN not set: Requires explicit `ENVIRONMENT=development` override

**When to use:** Pre-production testing

---

### Level 3: Production (Vault Required, Fails Without It)
```bash
ENVIRONMENT=production
VAULT_TOKEN=your-prod-token  # REQUIRED!
```

**Behavior:**
- ❌ If VAULT_TOKEN not set: **APPLICATION FAILS TO START**
- ⚠️ If Vault connection fails but .env available: Uses fallback with loud warnings
- ✅ If Vault works: No warnings

**When to use:** Live production deployment

---

## 📋 Environment Variable Reference

| Variable | Values | Purpose |
|----------|--------|---------|
| `ENVIRONMENT` | `development`, `staging`, `production` | Determines safety level |
| `REQUIRE_VAULT` | `true`, `false` | Explicitly require Vault (overrides `ENVIRONMENT`) |
| `VAULT_TOKEN` | Token string | Authentication to Vault |
| `VAULT_ADDR` | URL | Vault server address |

---

## 🎯 Behavior Matrix

| ENVIRONMENT | REQUIRE_VAULT | VAULT_TOKEN | Behavior |
|-------------|---------------|-------------|----------|
| `development` | - | Not set | ✅ Use `.env` (no warnings) |
| `development` | - | Set | ✅ Use Vault (with fallback) |
| `staging` | - | Not set | ⚠️ Requires override |
| `staging` | - | Set | ✅ Use Vault (with fallback) |
| `production` | - | Not set | ❌ **FAIL TO START** |
| `production` | - | Set | ✅ Use Vault (warnings if fallback) |
| Any | `true` | Not set | ❌ **FAIL TO START** |
| Any | `true` | Set | ✅ Use Vault |

---

## 🔍 Examples

### ✅ Local Development (Safe)
```bash
# Just use .env - no Vault needed
make up
make gradio

# No warnings!
```

### ⚠️ Testing with Local Vault
```bash
# Start local vault
make vault-dev-start
make vault-migrate

# Run with Vault
export VAULT_ADDR=http://vault-dev:8200
export VAULT_TOKEN=dev-root-token-agenticta
make restart

# Warns if Vault fails, falls back to .env
```

### ✅ Staging Deployment
```bash
# Get staging token
./scripts/vault/get_vault_token.sh staging
source .env.vault-staging

# Deploy (ENVIRONMENT=staging enforces Vault)
make up
```

### ❌ Production Without Vault (BLOCKED!)
```bash
export ENVIRONMENT=production
# No VAULT_TOKEN set

python gradioUI.py
# ❌ ERROR: PRODUCTION ERROR: VAULT_TOKEN not set!
#    ENVIRONMENT: production
#    Production deployments MUST use Vault for security.
```

### ✅ Production With Vault (Correct)
```bash
# Get production token
./scripts/vault/get_vault_token.sh prod
source .env.vault-prod

# Deploy
make deploy-prod

# ✅ Starts successfully, uses Vault
```

---

## 🚨 Error Messages

### Production Without Vault
```
❌ PRODUCTION ERROR: VAULT_TOKEN not set!
   ENVIRONMENT: production
   REQUIRE_VAULT: False
   Production deployments MUST use Vault for security.
   Set VAULT_TOKEN or set ENVIRONMENT=development for local dev.
```

**Solution:** 
```bash
# Option 1: Set token
export VAULT_TOKEN=your-token

# Option 2: Override for local testing (NOT recommended!)
export ENVIRONMENT=development
```

### Vault Fallback in Production
```
⚠️  VAULT FALLBACK: Using NVIDIA_API_KEY from environment variable.
    Vault unavailable or secret not found at agenticta/api-keys/nvidia_api_key.
    This is insecure for production!
```

**Solution:**
- Check Vault connectivity
- Verify secrets exist in Vault
- Check VAULT_TOKEN is valid
- Run `make vault-check` to diagnose

---

## 💡 Best Practices

### 1. **Always Set ENVIRONMENT**
```bash
# In docker-compose-prod.yml
environment:
  - ENVIRONMENT=production
```

### 2. **Use REQUIRE_VAULT for Critical Services**
```bash
# For services that MUST use Vault
environment:
  - REQUIRE_VAULT=true
  - VAULT_TOKEN=${VAULT_TOKEN}
```

### 3. **Test in Staging First**
```bash
# Never deploy to prod without staging validation!
ENVIRONMENT=staging make test-deployment
# Then...
ENVIRONMENT=production make deploy-prod
```

### 4. **Monitor Fallback Warnings**
Set up alerts for "VAULT FALLBACK" warnings in production logs - this indicates a problem!

---

## 🔧 Troubleshooting

### Q: How do I bypass Vault for local dev?
**A:** Don't set `VAULT_TOKEN` or set `ENVIRONMENT=development` (default)

### Q: Vault is set up but still getting warnings?
**A:** Check:
```bash
# Verify env vars
docker compose exec agenticta env | grep VAULT

# Check secrets in vault
make vault-check

# Run tests
make test
```

### Q: Need to temporarily disable Vault enforcement?
**A:** **NOT RECOMMENDED** but for emergencies:
```bash
# Override in docker-compose
environment:
  - ENVIRONMENT=development  # Bypasses enforcement
  - VAULT_TOKEN=  # Clear token
```

---

## 📚 Related Documentation

- **Quick Start**: `../QUICKSTART.md` - All modes with setup instructions
- **Vault Tiers**: `VAULT_TIERS.md` - Local, Staging, Production details
- **Vault Switching**: `VAULT_SWITCHING.md` - How to switch between modes
- **Vault Setup**: `../scripts/vault/README.md`

