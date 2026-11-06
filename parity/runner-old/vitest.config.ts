const old = process.env.OLD_NEXUS_DIR!;

export default {
  root: old,
  resolve: { alias: { "@": old } },
  server: { fs: { strict: false } },
  test: {
    globals: true,
    dir: import.meta.dirname,
    include: ["old-nexus.test.ts"],
    testTimeout: 30 * 60 * 1000,
    coverage: { enabled: false },
  },
};
