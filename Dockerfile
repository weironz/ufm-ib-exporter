FROM python:3.12-slim

WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY ufm_exporter.py .

EXPOSE 9627
CMD ["python", "ufm_exporter.py"]
