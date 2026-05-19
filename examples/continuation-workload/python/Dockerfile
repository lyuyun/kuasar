FROM python:3.12-slim

WORKDIR /app

COPY continuation.py /app/continuation.py
COPY continuation_server.py /app/continuation_server.py

RUN groupadd -r appuser \
    && useradd -r -u 10001 -g appuser appuser \
    && chmod 755 /app/*.py

USER 10001

ENV MODEL_NAME=llama3-simulated \
    LISTEN_ADDR=:8000

EXPOSE 8000

CMD ["python3", "/app/continuation_server.py"]
