import dotenv from 'dotenv';
import { startEventsServer } from './api/events-server';
import { EventSubscriber } from './services/event-subscriber';
import { NotificationScheduler } from './services/notification-scheduler';
import { ScheduledNotificationRepository } from './services/scheduled-notification-repository';
import { NotificationAPI } from './services/notification-api';
import { initializeDatabase } from './database/database';
import { DiscordNotificationService } from './services/discord-notification';
import { initNotificationAnalyticsAggregator } from './services/notification-analytics-aggregator';
import { NotificationMetricsStore } from './services/notification-metrics-store';
import { NotificationMetricsRunner } from './services/notification-metrics-runner';
import logger from './utils/logger';
import { loadConfig, ConfigError } from './config';

dotenv.config();

async function main() {
  const config = loadConfig();

  let scheduler: NotificationScheduler | null = null;
  let notificationAPI: NotificationAPI | null = null;
  let metricsRunner: NotificationMetricsRunner | null = null;
  let metricsStore: NotificationMetricsStore | null = null;

  const needDb =
    config.scheduler?.enabled ||
    config.rateLimit?.enabled ||
    config.analytics?.enabled ||
    config.cleanup?.enabled;

  if (config.analytics?.enabled) {
    initNotificationAnalyticsAggregator(config.analytics);
  }

  if (needDb) {
    try {
      logger.info('Initializing database');
      const db = await initializeDatabase(config.databasePath);

      if (config.analytics?.enabled) {
        metricsStore = new NotificationMetricsStore(db);
        metricsRunner = new NotificationMetricsRunner(config.analytics, metricsStore);
        await metricsRunner.start();
        logger.info('Notification metrics runner started successfully');
      }

      if (config.scheduler?.enabled) {
        const repository = new ScheduledNotificationRepository(db);
        notificationAPI = new NotificationAPI(repository);

        let discordService: DiscordNotificationService | null = null;
        if (config.discord) {
          discordService = new DiscordNotificationService(config.discord);
        }

        scheduler = new NotificationScheduler(repository, config.scheduler, discordService);
        await scheduler.start();

        logger.info('Notification scheduler started successfully');
      }
    } catch (error) {
      logger.error('Failed to initialize database or background workers', { error });
      throw error;
    }
  }

  const eventsServer = startEventsServer({
    port: config.eventsApiPort,
    corsOrigin: config.eventsApiCorsOrigin,
    stellarRpcUrl: config.stellarRpcUrl,
    discordWebhookUrl: config.discord?.webhookUrl,
    notificationAPI,
    rateLimit: config.rateLimit,
    metricsStore,
  });

  const subscriber = new EventSubscriber(config);
  await subscriber.start();

  const shutdown = async () => {
    logger.info('Shutting down services...');

    if (metricsRunner) {
      await metricsRunner.stop();
    }

    if (scheduler) {
      await scheduler.stop();
    }

    await subscriber.stop();
    eventsServer.close();

    logger.info('All services stopped successfully');
    process.exit(0);
  };

  process.on('SIGINT', async () => {
    logger.info('Received SIGINT, shutting down');
    await shutdown();
  });

  process.on('SIGTERM', async () => {
    logger.info('Received SIGTERM, shutting down');
    await shutdown();
  });
}

main().catch((err) => {
  if (err instanceof ConfigError) {
    logger.error('Configuration error', { error: err.message });
  } else {
    logger.error('Error starting service', { error: err });
  }
  process.exit(1);
});
