// packages/web/web-search-exa/src/index.ts
import { launchEnvironmentOf } from "@deepseek-ai/dsh-launch-environment";
import z from "@deepseek-ai/schemastery";

// packages/web/web-search-exa/src/provider.ts
import { WebError } from "@deepseek-ai/dsh-web";
var EXA_PROVIDER_ID = "exa";
var EXA_DEFAULT_BASE_URL = "https://api.exa.ai";
var EXA_DEFAULT_SEARCH_TYPE = "auto";
var EXA_DEFAULT_HIGHLIGHTS_PER_RESULT = 1;
var USER_AGENT = "deepseek-harness/0.0.1";
function mapExaResult(result) {
  const snippet = result.highlights?.find((highlight) => highlight.trim().length > 0);
  if (snippet === void 0) return void 0;
  return {
    url: result.url,
    ...result.title != null && result.title.length > 0 ? { title: result.title } : {},
    snippet,
    ...result.publishedDate != null && result.publishedDate.length > 0 ? { publishedAt: result.publishedDate } : {}
  };
}
function mapExaResponse(response) {
  const sources = (response.results ?? []).map(mapExaResult).filter((source) => source !== void 0);
  return { sources, truncated: false };
}
var ExaSearchProvider = class {
  constructor(options) {
    this.options = options;
  }
  options;
  id = EXA_PROVIDER_ID;
  available() {
    return this.options.apiKey.length > 0 && isValidBaseUrl(this.options.baseURL) && isPositiveInteger(this.options.highlightsPerResult) && (this.options.numResults === void 0 || isPositiveInteger(this.options.numResults));
  }
  async search(request, signal) {
    const numResults = request.maxResults ?? this.options.numResults;
    let response;
    try {
      response = await fetch(`${this.options.baseURL}/search`, {
        method: "POST",
        redirect: "error",
        headers: {
          "authorization": `Bearer ${this.options.apiKey}`,
          "content-type": "application/json",
          "accept": "application/json",
          "user-agent": USER_AGENT
        },
        body: JSON.stringify({
          query: request.query,
          type: this.options.searchType,
          contents: { highlights: { highlightsPerUrl: this.options.highlightsPerResult } },
          ...numResults !== void 0 ? { numResults } : {}
        }),
        ...signal !== void 0 ? { signal } : {}
      });
    } catch (error) {
      if (isAbortError(error)) throw new WebError("Exa search aborted", "WEB_ABORTED", { cause: error });
      throw new WebError(`Exa search request failed: ${String(error)}`, "WEB_PROVIDER_ERROR", { cause: error });
    }
    if (!response.ok) {
      const status = response.status;
      let message = `Exa API error (HTTP ${status})`;
      try {
        const parsed = await response.json();
        const detail = parsed.error ?? parsed.message;
        if (detail !== void 0 && detail.length > 0) message = detail;
      } catch (error) {
        if (isAbortError(error)) throw new WebError("Exa search aborted", "WEB_ABORTED", { cause: error });
      }
      throw new WebError(message, "WEB_PROVIDER_ERROR");
    }
    try {
      const payload = await response.json();
      return mapExaResponse(payload);
    } catch (error) {
      if (isAbortError(error)) throw new WebError("Exa search aborted", "WEB_ABORTED", { cause: error });
      throw new WebError(`Exa returned an unprocessable response body: ${String(error)}`, "WEB_PROVIDER_ERROR", { cause: error });
    }
  }
};
function isValidBaseUrl(baseURL) {
  return URL.canParse(baseURL);
}
function isPositiveInteger(value) {
  return Number.isInteger(value) && value > 0;
}
function isAbortError(error) {
  return error instanceof DOMException && error.name === "AbortError";
}

// packages/web/web-search-exa/src/index.ts
var name = "web-search-exa";
var inject = ["web"];
var Config = z.object({
  apiKey: z.string(),
  baseURL: z.string(),
  searchType: z.union(["auto", "keyword", "neural"]),
  numResults: z.number().step(1).min(1),
  highlightsPerResult: z.number().step(1).min(1)
});
function apply(ctx, config) {
  ctx.web.registerSearchProvider(new ExaSearchProvider({
    // Every environment layer may name this key: the product trusts the
    // project it is launched in, and the managed store is not involved here.
    apiKey: config.apiKey ?? launchEnvironmentOf(ctx).get("EXA_API_KEY")?.value ?? "",
    baseURL: config.baseURL ?? EXA_DEFAULT_BASE_URL,
    searchType: config.searchType ?? EXA_DEFAULT_SEARCH_TYPE,
    highlightsPerResult: config.highlightsPerResult ?? EXA_DEFAULT_HIGHLIGHTS_PER_RESULT,
    ...config.numResults !== void 0 ? { numResults: config.numResults } : {}
  }));
}
export {
  Config,
  EXA_DEFAULT_BASE_URL,
  EXA_DEFAULT_HIGHLIGHTS_PER_RESULT,
  EXA_DEFAULT_SEARCH_TYPE,
  EXA_PROVIDER_ID,
  ExaSearchProvider,
  apply,
  inject,
  name
};
