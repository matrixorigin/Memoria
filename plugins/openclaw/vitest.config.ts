import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["openclaw/__tests__/**/*.test.ts"],
    environment: "node",
    clearMocks: true,
    restoreMocks: true,
  },
});
