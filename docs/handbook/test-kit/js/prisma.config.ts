import { defineConfig } from "prisma/config";
export default defineConfig({
  schema: "prisma/schema.prisma",
  datasource: { url: process.env.APP_URL ?? "postgresql://localhost/unused" },
});
