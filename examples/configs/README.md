# Example Configuration Files

This directory contains example configuration files for different LLM providers.

## Quick Setup

Choose a configuration file based on your provider and copy it to `~/.config/llm-git/config.toml`:

```bash
# LiteLLM (local proxy)
cp examples/configs/litellm.toml ~/.config/llm-git/config.toml

# Anthropic Direct
cp examples/configs/anthropic.toml ~/.config/llm-git/config.toml
export LLM_GIT_API_KEY=sk-ant-...

# OpenRouter
cp examples/configs/openrouter.toml ~/.config/llm-git/config.toml
export LLM_GIT_API_KEY=sk-or-...

# OpenAI
cp examples/configs/openai.toml ~/.config/llm-git/config.toml
export LLM_GIT_API_KEY=sk-...
```

## Configuration Files

### litellm.toml
For local LiteLLM proxy server. Best for development and testing.

**Setup:**
```bash
pip install litellm
export ANTHROPIC_API_KEY=your_key
litellm --port 4000 --model claude-sonnet-4.5
```

### anthropic.toml
Direct connection to Anthropic's API. Best for production use with Claude models.

**Requirements:**
- Anthropic API key from https://console.anthropic.com/

### openrouter.toml
Access multiple model providers through OpenRouter. Best for flexibility.

**Requirements:**
- OpenRouter API key from https://openrouter.ai/keys
- Supports Claude, GPT, Gemini, and many other models

### openai.toml
Direct connection to OpenAI's API. Best for GPT models.

**Requirements:**
- OpenAI API key from https://platform.openai.com/api-keys

## Customization

All config files can be customized with:

- **Models**: Change `analysis_model` and `summary_model`
- **Timeouts**: Adjust `request_timeout_secs` and `connect_timeout_secs`
- **Commit limits**: Modify `summary_guideline`, `summary_soft_limit`, `summary_hard_limit`
- **Retries**: Configure `max_retries` and `initial_backoff_ms`
- **Temperature**: Adjust model randomness (0.0 = deterministic, 1.0 = creative)

See the main README.md for full configuration documentation.
