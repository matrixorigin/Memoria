FROM python:3.11-slim

RUN useradd -m -u 1000 appuser

WORKDIR /app

RUN sed -i 's/deb.debian.org/mirrors.tuna.tsinghua.edu.cn/g' /etc/apt/sources.list.d/debian.sources && \
    apt-get update && apt-get install -y gcc && rm -rf /var/lib/apt/lists/*

# Install dependencies first (cached unless pyproject.toml changes)
COPY pyproject.toml ./
RUN pip install --no-cache-dir -i https://pypi.tuna.tsinghua.edu.cn/simple/ ".[openai-embedding]" && \
    apt-get purge -y gcc && apt-get autoremove -y

# Copy source + README (needed for package metadata)
COPY memoria/ memoria/
COPY README.md ./
RUN pip install --no-cache-dir --no-deps -i https://pypi.tuna.tsinghua.edu.cn/simple/ .

RUN chown -R appuser:appuser /app
USER appuser

EXPOSE 8000

CMD ["python", "-m", "uvicorn", "memoria.api.main:app", "--host", "0.0.0.0", "--port", "8000"]
