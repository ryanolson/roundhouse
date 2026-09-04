# Vault Configuration Tiers

AgenticTA supports 3 Vault configurations for different environments.

## 📋 Quick Reference

| Tier | Server Address | Use Case | Setup Command |
|------|---------------|----------|---------------|
| **Local** | `http://vault-dev:8200` | Local development, testing | `make vault-dev-start` |
| **Staging** | `https://stg.internal.vault.nvidia.com` | Pre-production testing | `./scripts/vault/get_vault_token.sh staging` |
| **Production** | `https://prod.internal.vault.nvidia.com` | Live deployment | `./scripts/vault/get_vault_token.sh prod` |

---

## 🔧 Tier 1: Local (vault-dev)

**Configuration:** `env.vault-dev.example`

```bash
VAULT_ADDR=http://vault-dev:8200
VAULT_TOKEN=dev-root-token-agenticta
VAULT_NAMESPACE=
```

**Setup:**
```bash
# Start local vault
make vault-dev-start

# Migrate secrets
make vault-migrate

# Use local vault
export VAULT_ADDR=http://vault-dev:8200
export VAULT_TOKEN=dev-root-token-agenticta
make restart
```

**Use for:**
- ✅ Local development
- ✅ Testing Vault integration
- ✅ Offline work
- ✅ Learning

**DO NOT use for:**
- ❌ Production
- ❌ Shared/team environments
- ❌ Real secrets

---

## 🧪 Tier 2: Staging (NVIDIA)

**Configuration:** `env.vault-staging.example`

```bash
VAULT_ADDR=https://stg.internal.vault.nvidia.com
VAULT_NAMESPACE=wwfo-self-ta
VAULT_TOKEN=<get-from-oidc>
```

**Setup:**
```bash
# Get staging token
./scripts/vault/get_vault_token.sh staging

# Copy and configure
cp env.vault-staging.example .env.vault-staging
# Token is automatically added by get_vault_token.sh

# Use staging vault
source .env.vault-staging
make restart
```

**Use for:**
- ✅ Pre-production testing
- ✅ Integration testing
- ✅ Team development
- ✅ CI/CD pipelines
- ✅ Final validation before production

**DO NOT use for:**
- ❌ Production deployments
- ❌ Customer-facing services

---

## 🏢 Tier 3: Production (NVIDIA)

**Configuration:** `env.vault-prod.example`

```bash
VAULT_ADDR=https://prod.internal.vault.nvidia.com
VAULT_NAMESPACE=wwfo-self-ta
VAULT_TOKEN=<get-from-oidc>
```

**Setup:**
```bash
# ⚠️ WARNING: Production! Test in staging first!

# Get production token
./scripts/vault/get_vault_token.sh prod

# Copy and configure
cp env.vault-prod.example .env.vault-prod
# Token is automatically added by get_vault_token.sh

# Deploy with production vault
source .env.vault-prod
make deploy-prod
```

**Use for:**
- ✅ Production deployments ONLY
- ✅ Customer-facing services
- ✅ Live environments

**Requirements:**
- ⚠️ Must test in staging first
- ⚠️ Token renewal configured
- ⚠️ Monitoring enabled
- ⚠️ Proper access controls

---

## 🔄 Switching Between Tiers

```bash
# Switch to local
unset VAULT_TOKEN
export VAULT_ADDR=http://vault-dev:8200
export VAULT_TOKEN=dev-root-token-agenticta
make restart

# Switch to staging
source .env.vault-staging
make restart

# Switch to production (CAREFUL!)
source .env.vault-production
make deploy-prod
```

---

## 🛡️ Security Notes

### What's Secret?
- ✅ `VAULT_TOKEN` - **KEEP PRIVATE!**
- ❌ `VAULT_ADDR` - Not secret (just server address)
- ❌ `VAULT_NAMESPACE` - Not secret (just namespace name)

### Best Practices
1. **Never commit tokens** to git
2. **Rotate tokens regularly** (especially production)
3. **Use appropriate tier** for your use case
4. **Test in staging** before production
5. **Monitor token expiration** in production

---

## 📊 Decision Tree

```
Are you developing locally?
├─ Yes → Use Local (vault-dev)
└─ No
    └─ Is this for production?
        ├─ No → Use Staging
        └─ Yes
            └─ Have you tested in staging?
                ├─ No → Use Staging first!
                └─ Yes → Use Production (carefully!)
```

---

## 🚨 Common Issues

### "Cannot connect to Vault"
```bash
# Check which tier you're using
echo $VAULT_ADDR

# Local: Is vault-dev running?
docker ps | grep vault

# Staging/Prod: Are you on VPN?
ping stg.internal.vault.nvidia.com
```

### "Token expired"
```bash
# Get new token
./scripts/vault/get_vault_token.sh <staging|prod>

# Or renew existing token
vault token renew
```

### "Secrets not found"
```bash
# Check vault
make vault-check

# Migrate if needed (local/staging only)
make vault-migrate
```

---

## 📚 Additional Documentation

- **Quick Start**: `../QUICKSTART.md` - All modes with setup instructions
- **Vault Switching**: `VAULT_SWITCHING.md` - How to switch between modes
- **Vault Scripts**: `../scripts/vault/README.md`
- **NVIDIA Vault**: `../scripts/vault/NVIDIA_VAULT_SETUP.md`
- **Token Renewal**: `../scripts/vault/TOKEN_RENEWAL_GUIDE.md`

