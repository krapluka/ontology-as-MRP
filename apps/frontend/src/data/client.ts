import { Client, cacheExchange, fetchExchange } from "urql";

/**
 * The urql client for the plant-pulse-api read surface. Nothing is cached beyond a request
 * round-trip: after a command the affected reads are re-fetched (see the data hooks), so the
 * view does not show stale state.
 */
export function createGraphqlClient(url = "/api/graphql"): Client {
  return new Client({
    url,
    exchanges: [cacheExchange, fetchExchange],
    requestPolicy: "network-only",
  });
}
