//! Benchmark: Dockerfile parsing performance.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use dockerfile_parser::parse_dockerfile;

fn bench_simple_dockerfile(c: &mut Criterion) {
    let dockerfile = r#"
FROM ubuntu:20.04
RUN apt-get update && apt-get install -y curl
ENV APP_HOME=/app
WORKDIR /app
COPY . /app
RUN make build
CMD ["./app"]
"#;
    c.bench_function("parse_simple_dockerfile", |b| {
        b.iter(|| parse_dockerfile(black_box(dockerfile)).unwrap())
    });
}

fn bench_multistage_dockerfile(c: &mut Criterion) {
    let dockerfile = r#"
FROM golang:1.24 AS builder
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -o /app

FROM alpine:3.18
RUN apk add --no-cache ca-certificates
COPY --from=builder /app /usr/local/bin/app
ENTRYPOINT ["/usr/local/bin/app"]
"#;
    c.bench_function("parse_multistage_dockerfile", |b| {
        b.iter(|| parse_dockerfile(black_box(dockerfile)).unwrap())
    });
}

fn bench_complex_dockerfile(c: &mut Criterion) {
    let dockerfile = r#"
FROM ubuntu:20.04 AS base
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl \
    wget \
    git \
    build-essential \
    && rm -rf /var/lib/apt/lists/*
ENV PATH="/usr/local/bin:${PATH}"
WORKDIR /app

FROM base AS deps
COPY package*.json ./
RUN npm ci --only=production

FROM base AS builder
COPY --from=deps /app/node_modules ./node_modules
COPY . .
RUN npm run build

FROM base AS runtime
COPY --from=builder /app/dist ./dist
COPY --from=builder /app/package.json ./
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=3s --retries=3 CMD curl -f http://localhost:3000/health
USER node
ENTRYPOINT ["node", "dist/index.js"]
"#;
    c.bench_function("parse_complex_dockerfile", |b| {
        b.iter(|| parse_dockerfile(black_box(dockerfile)).unwrap())
    });
}

fn bench_variable_substitution(c: &mut Criterion) {
    let mut group = c.benchmark_group("variable_substitution");
    for size in [1, 10, 50, 100] {
        let dockerfile = (0..size)
            .map(|i| format!("ARG VAR{}=value{}\nENV APP_{}=$VAR{}", i, i, i, i))
            .collect::<Vec<_>>()
            .join("\n");
        let full = format!("FROM ubuntu:20.04\n{}", dockerfile);
        group.bench_with_input(BenchmarkId::from_parameter(size), &full, |b, input| {
            b.iter(|| parse_dockerfile(black_box(input)).unwrap())
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_simple_dockerfile,
    bench_multistage_dockerfile,
    bench_complex_dockerfile,
    bench_variable_substitution,
);
criterion_main!(benches);