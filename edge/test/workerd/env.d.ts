/// <reference types="@cloudflare/vitest-pool-workers" />

declare module "cloudflare:test" {
  interface ProvidedEnv {
    TEST_LOG: DurableObjectNamespace;
    TEST_CHAT: DurableObjectNamespace;
    TEST_REGISTRY: DurableObjectNamespace;
    TEST_DEVICE: DurableObjectNamespace;
    VAULT_ROOMS: DurableObjectNamespace;
  }
}
