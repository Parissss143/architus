import os
from datetime import datetime, timedelta
from concurrent.futures import ThreadPoolExecutor
import time
import asyncio

from lib.config import logger, domain_name
from lib.hoar_frost import HoarFrostGenerator

import grpc
import manager_pb2_grpc as manager_grpc
import manager_pb2 as message

# TODO: Add some thread safety locks here and there
# TODO: Maybe increase sleep on health_check to decrease races for lock
#       that I have to add.

class Manager(manager_grpc.ManagerServicer):
    """
    Implements a server for the Manager gRPC protocol.
    """

    def __init__(self, total_shards):
        """
        Instantiates a new manager server that handles some
        number of shards.
        """
        logger.info(f"Number of shards: {total_shards}")
        self.hoarfrost_gen = HoarFrostGenerator()
        self.total_shards = total_shards
        self.registered = [False for _ in range(total_shards)]
        self.last_checkin = dict()
        self.store = dict()

    async def health_check(self):
        while True:
            time.sleep(5)
            for shard, last_checkin in self.last_checkin.items():
                if last_checkin is not None and last_checkin < datetime.now() - timedelta(seconds=5):
                    logger.error(f"--- SHARD {shard} MISSED ITS HEARTBEAT, DEREGISTERING... ---")
                    self.registered[shard] = False
                    self.last_checkin[shard] = None

    def register(self, request, context):
        """Returns the next shard id that needs to be filled as well as the total shards"""
        if all(self.registered):
            raise Exception("Shard trying to register even though we're full")
        i = next(i for i in range(self.total_shards) if not self.registered[i])
        logger.info(f"Shard requested id, assigning {i + 1}/{self.total_shards}...")
        self.registered[i] = True
        return message.ShardInfo(shard_id=i, shard_count=self.total_shards)

    def guild_count(self, request, context):
        """Return guild and user count information"""
        gc = 0
        uc = 0
        for guilds in self.store.values():
            gc += len(guilds)
            for guild in guilds:
                uc += guild.member_count
        return message.GuildInfo(guild_count=gc, user_count=uc)

    def checkin(self, request, context):
        self.last_checkin[request.shard_id] = datetime.now()
        self.registered[shard_id] = True
        return message.Void(val=True)

    def publish_file(self, request, context):
        """Missing associated documentation comment in .proto file"""
        assert (len(data) > 0)
        filetype = "png" if request.filetype == "" else request.filetype
        if request.name == "":
            name = str(self.hoarfrost_gen.generate())
        location = request.location
        if location == "":
            location = "assets"
        directory = f"/var/www/{location}"

        if not os.path.exists(directory):
            os.makedirs(directory)
        with open(f"{directory}/{name}.{filetype}", "wb") as f:
            logger.info(f"Writing {directory}/{filename}.{filetype}")
            f.write(requests.file)
        
        return message.Url(Url=f"https://cdn.{domain_name}/{location}/{name}.{filetype}")

    def all_guilds(self, request, context):
        """Return information about all guilds that the bot is in, including their admins"""
        for guilds in self.store.values():
            for guild in guilds:
                yield guild

    def guild_update(self, request_iterator, context):
        """Update the manager with the latest information about a shard's guilds"""
        guilds = []
        for guild in request_iterator:
            guilds.append(guild)
        if len(guilds) == 0:
            return message.Void(val=False)
        logger.debug(f"Received guild list from shard {guilds[0].shard_id} of {len(guilds)} guilds")
        self.store[guilds[0].shard_id] = guilds
        return message.Void(val=True)

def serve(manager):
    server = grpc.server(ThreadPoolExecutor(max_workers=20))
    manager_grpc.add_ManagerServicer_to_server(manager, server)
    server.add_insecure_port("0.0.0.0:50051")
    server.add_insecure_port("manager:50051")
    server.start()
    logger.debug("gRPC server started")

if __name__ == "__main__":
    manager = Manager(int(os.environ["NUM_SHARDS"]))
    serve(manager)
    asyncio.run(manager.health_check())
