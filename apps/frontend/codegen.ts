import type { CodegenConfig } from "@graphql-codegen/cli";

// Generates the typed GraphQL client from the committed schema snapshot and the inbox
// operations. Offline and deterministic: the same schema.graphql and operations reproduce
// the same output, so the regen script can be checked without a backend.
const config: CodegenConfig = {
  schema: "schema.graphql",
  documents: ["src/data/operations.graphql"],
  ignoreNoDocuments: true,
  generates: {
    "src/data/graphql/": {
      preset: "client",
      presetConfig: { fragmentMasking: false },
      config: {
        useTypeImports: true,
        skipTypename: true,
      },
    },
  },
};

export default config;
