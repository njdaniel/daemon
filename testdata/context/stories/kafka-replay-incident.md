# Recovering six hours of events with Kafka replay

At Contoso a downstream consumer shipped a bug that silently dropped shipment events for six
hours. Because the pipeline kept every event in Kafka with idempotent consumers, we fixed the
consumer, reset its offset to the start of the bad window and replayed. Dispatchers saw correct
estimates again within the hour and no customer data was lost. Afterwards I added a lag alert on
every consumer group so a stalled or skipping consumer pages within minutes, not hours.
