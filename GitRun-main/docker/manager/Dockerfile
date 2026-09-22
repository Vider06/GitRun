FROM python:3.12-slim-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends docker.io ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /opt/gitrun
COPY autoscaler/gitrun_manager.py /opt/gitrun/gitrun_manager.py
COPY autoscaler/gitrun_updater_utility.py /opt/gitrun/gitrun_updater_utility.py

CMD ["python3", "/opt/gitrun/gitrun_manager.py"]
