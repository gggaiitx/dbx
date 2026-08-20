import { describe, expect, it } from "vitest";
import { buildConnectionUrlFromConfig } from "@/lib/connection/connectionUrl";

describe("buildConnectionUrlFromConfig", () => {
  it("builds a MySQL URL including default port and credentials", () => {
    const url = buildConnectionUrlFromConfig({
      db_type: "mysql",
      host: "db.example.com",
      port: 3306,
      username: "root",
      password: "secret",
      database: "app",
      url_params: "",
      ssl: false,
    });
    expect(url).toBe("mysql://root:secret@db.example.com/app");
  });

  it("omits a redundant default port", () => {
    const url = buildConnectionUrlFromConfig({
      db_type: "postgres",
      host: "pg.example.com",
      port: 0,
      username: "",
      password: "",
      database: undefined,
      url_params: "",
      ssl: false,
    });
    expect(url).toBe("postgresql://pg.example.com");
  });

  it("keeps raw characters (e.g. @) in the userinfo instead of percent-encoding", () => {
    const url = buildConnectionUrlFromConfig({
      db_type: "postgres",
      host: "192.168.10.72",
      port: 5432,
      username: "postgres",
      password: "@Medic0m",
      database: "MedicomPIP2DB",
      url_params: "sslmode=disable",
      ssl: false,
    });
    expect(url).toBe("postgresql://postgres:@Medic0m@192.168.10.72/MedicomPIP2DB?sslmode=disable");
  });

  it("appends a query parameter when ssl-mode is required for MySQL", () => {
    const url = buildConnectionUrlFromConfig({
      db_type: "mysql",
      host: "db.example.com",
      port: 3306,
      username: "root",
      password: "",
      database: "app",
      url_params: "charset=utf8mb4",
      ssl: true,
    });
    expect(url).toBe("mysql://root@db.example.com/app?charset=utf8mb4&ssl-mode=required");
  });

  it("switches to rediss when Redis SSL is enabled", () => {
    const url = buildConnectionUrlFromConfig({
      db_type: "redis",
      host: "cache.example.com",
      port: 6379,
      username: "",
      password: "p@ss",
      database: undefined,
      url_params: "",
      ssl: true,
    });
    expect(url).toBe("rediss://:p@ss@cache.example.com");
  });

  it("builds a MongoDB URL with a database path", () => {
    const url = buildConnectionUrlFromConfig({
      db_type: "mongodb",
      host: "mongo.example.com",
      port: 27017,
      username: "admin",
      password: "",
      database: "shop",
      url_params: "",
      ssl: false,
    });
    expect(url).toBe("mongodb://admin@mongo.example.com/shop");
  });

  it("uses the https scheme for HTTP datastores when TLS is on", () => {
    const url = buildConnectionUrlFromConfig({
      db_type: "elasticsearch",
      host: "es.example.com",
      port: 443,
      username: "",
      password: "",
      database: undefined,
      url_params: "",
      ssl: true,
    });
    expect(url).toBe("https://es.example.com");
  });

  it("builds an Oracle JDBC thin URL for a service name", () => {
    const url = buildConnectionUrlFromConfig({
      db_type: "oracle",
      host: "ora.example.com",
      port: 1521,
      username: "sys",
      password: "secret",
      database: "ORCLPDB",
      url_params: "",
      ssl: false,
      oracle_connection_type: "service_name",
    });
    expect(url).toBe("jdbc:oracle:thin:@//ora.example.com/ORCLPDB");
  });

  it("builds an Oracle JDBC thin URL preserving the SID", () => {
    const url = buildConnectionUrlFromConfig({
      db_type: "oracle",
      host: "ora.example.com",
      port: 1521,
      username: "sys",
      password: "secret",
      database: "ORCL",
      url_params: "",
      ssl: false,
      oracle_connection_type: "sid",
    });
    expect(url).toBe("jdbc:oracle:thin:@ora.example.com:ORCL");
  });

  it("builds a SQL Server URL including the database path", () => {
    const url = buildConnectionUrlFromConfig({
      db_type: "sqlserver",
      host: "sql.example.com",
      port: 1433,
      username: "sa",
      password: "secret",
      database: "master",
      url_params: "",
      ssl: false,
    });
    expect(url).toBe("mssql://sa:secret@sql.example.com/master");
  });

  it("returns undefined when URL cannot be derived (local file / cloud)", () => {
    for (const db_type of ["sqlite", "duckdb", "access", "dynamodb", "turso", "cloudflare-d1", "bigquery", "jdbc", "nacos", "consul"] as const) {
      expect(buildConnectionUrlFromConfig({ db_type, host: "x", port: 1, username: "", password: "", database: undefined, url_params: "", ssl: false })).toBeUndefined();
    }
  });

  it("returns undefined when the host is empty or a multi-host list", () => {
    expect(buildConnectionUrlFromConfig({ db_type: "mysql", host: "", port: 3306, username: "", password: "", database: undefined, url_params: "", ssl: false })).toBeUndefined();
    expect(buildConnectionUrlFromConfig({ db_type: "mysql", host: "a:3306,b:3307", port: 0, username: "", password: "", database: undefined, url_params: "", ssl: false })).toBeUndefined();
  });

  it("round-trips built URLs back into their source fields", () => {
    const source = { db_type: "mysql" as const, host: "db.example.com", port: 3306, username: "root", password: "secret", database: "app", url_params: "", ssl: false };
    const url = buildConnectionUrlFromConfig(source)!;
    expect(url).toContain("db.example.com");
    expect(url).toContain("root:secret");
  });
});
